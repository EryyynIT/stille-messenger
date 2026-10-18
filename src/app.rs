//! Сшивка: CoreLink-поток → Message → Store → view.
//! Строится по зонам из GUI_ARCHITECTURE.md §1.

use std::sync::Arc;

use iced::{Subscription, Task};

use crate::core_link::mock::{MockLink, Scene};
use crate::core_link::uds::UdsLink;
use crate::core_link::{CoreLink, Request};
use crate::model::msg::MsgId;
use crate::ui::{self, LinkIn, MeetIn, Message, WindowIn};

fn update<L: CoreLink>(state: &mut App<L>, message: Message) -> Task<Message> {
    ui::update(state, message)
}

fn view<L: CoreLink>(state: &App<L>) -> iced::Element<'_, Message> {
    ui::view(state)
}

/// Канал демо-режима (для тестов и `--mock`); рабочий — [`UdsLink`].
pub type Link = MockLink;

/// Стейт программы.
pub type App<L> = ui::State<L>;

pub fn run() -> iced::Result {
    // Реальный канал — по умолчанию; `--mock` — демо-сцена без ядра.
    if std::env::args().any(|arg| arg == "--mock") {
        program(MockLink::new(Scene::demo())).run()
    } else {
        program(UdsLink::new()).run()
    }
}

/// Собирает Program приложения: один и тот же для окна и для iced_test.
pub fn program<L: CoreLink>(
    link: L,
) -> iced::application::Application<
    impl iced::Program<State = App<L>, Message = Message, Theme = iced::Theme>,
> {
    iced::application(Boot(Arc::new(link)), update, view)
        .title("Stille Messenger")
        .theme(theme)
        .window(iced::window::Settings {
            size: iced::Size::new(1000.0, 700.0),
            min_size: Some(iced::Size::new(800.0, 560.0)),
            icon: Some(window_icon()),
            ..Default::default()
        })
        .subscription(subscription)
}

/// Иконка окна из вшитого RGBA-ассета Stille.
fn window_icon() -> iced::window::Icon {
    // Распаковка не может упасть: ассет ровно 256×256×4 байта, что
    // проверено генератором и тестом длины файла.
    iced::window::icon::from_rgba(include_bytes!("../assets/icon.rgba").to_vec(), 256, 256)
        .expect("вшитая иконка корректна: 256×256×4 байта")
}

/// Загрузчик приложения: держит линк для boot.
struct Boot<L>(Arc<L>);

impl<L: CoreLink> iced::application::BootFn<App<L>, Message> for Boot<L> {
    fn boot(&self) -> (App<L>, Task<Message>) {
        boot(Arc::clone(&self.0))
    }
}

fn boot<L: CoreLink>(link: Arc<L>) -> (App<L>, Task<Message>) {
    let state = ui::State::new(Arc::clone(&link));
    // Бутстрап из CORE-API.md §8: контакты + зонд версии (от неё гейтятся
    // ядерные фичи UI: поиск 2.1+, закрепы 2.2+).
    let task = Task::batch([
        link_task(Arc::clone(&link), Request::Contacts, None),
        link_task(link, Request::Version, None),
    ]);
    (state, task)
}

/// Постоянный поток событий ядра + клавиатура (Esc закрывает оверлей)
/// + фокус окна (открытый в фокусе чат не уведомляет, GUI_UI.md §4)
/// + тик таймера разговора (S7) — только пока звонок активен.
fn subscription<L: CoreLink>(state: &App<L>) -> Subscription<Message> {
    let mut subs = vec![
        Subscription::run_with(Arc::clone(&state.link), events_subscription),
        esc_subscription(),
        focus_subscription(),
    ];
    if state.call_active() {
        subs.push(
            iced::time::every(std::time::Duration::from_secs(1))
                .map(|_| Message::Call(ui::CallIn::Tick)),
        );
    }
    Subscription::batch(subs)
}

fn events_subscription<L: CoreLink>(link: &Arc<L>) -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::StreamExt;
    CoreLink::events(Arc::clone(link)).map(|event| Message::Link(LinkIn::Event(event)))
}

/// Горячие клавиши живого окна (GUI_UI.md §4): Esc — закрыть оверлей,
/// Ctrl+1..9 — чат по номеру, Ctrl+Q — выход. Ctrl-комбинации — по
/// физическим клавишам, раскладка не важна. `listen()` отдаёт только
/// события, которые не съел виджет; пути композера дублированы в
/// key_binding (headless-эмулятор подписку не получает).
fn esc_subscription() -> Subscription<Message> {
    use iced::keyboard::key::{Code, Named, Physical};
    use iced::keyboard::{Event as KeyEvent, Key};
    iced::keyboard::listen().filter_map(|event| match event {
        KeyEvent::KeyPressed {
            key,
            physical_key,
            modifiers,
            ..
        } => {
            if let Key::Named(Named::Escape) = key {
                return Some(Message::Meet(MeetIn::CloseOverlay));
            }
            if modifiers.control() {
                if let Physical::Code(code) = physical_key {
                    if let Some(index) = ui::chat_index(&code) {
                        return Some(Message::Chat(ui::ChatIn::SelectIndex(index)));
                    }
                    if code == Code::KeyQ {
                        return Some(Message::Window(WindowIn::Exit));
                    }
                }
            }
            None
        }
        _ => None,
    })
}

/// Фокус окна для правила уведомлений (GUI_UI.md §4): без фокуса
/// уведомляется любой чат, включая открытый.
fn focus_subscription() -> Subscription<Message> {
    use iced::window::Event as WindowEvent;
    iced::event::listen().filter_map(|event| match event {
        iced::Event::Window(WindowEvent::Focused) => Some(Message::Window(WindowIn::Focus(true))),
        iced::Event::Window(WindowEvent::Unfocused) => {
            Some(Message::Window(WindowIn::Focus(false)))
        }
        _ => None,
    })
}

fn theme<L: CoreLink>(_: &App<L>) -> iced::Theme {
    iced::Theme::Dark
}

/// Разовый запрос ядру; ответ возвращается через Replied.
fn link_task<L: CoreLink>(link: Arc<L>, request: Request, tag: Option<MsgId>) -> Task<Message> {
    let reply_request = request.clone();
    Task::perform(link.execute(request), move |reply| {
        Message::Link(LinkIn::Replied {
            request: reply_request.clone(),
            reply,
            tag,
        })
    })
}

#[cfg(test)]
mod tests {
    /// Ассет иконки обязан совпадать по размеру с заявленным в window_icon:
    /// иначе распаковка упадёт уже в рантайме у пользователя.
    #[test]
    fn embedded_icon_is_256_squared_rgba() {
        let bytes = include_bytes!("../assets/icon.rgba");
        assert_eq!(bytes.len(), 256 * 256 * 4, "иконка ровно 256×256×4");
        let icon =
            iced::window::icon::from_rgba(include_bytes!("../assets/icon.rgba").to_vec(), 256, 256);
        assert!(icon.is_ok(), "распаковка успешна");
    }
}
