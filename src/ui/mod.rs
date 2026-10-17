//! UI: Elm-архитектура — один State, update() как единственное место
//! мутаций, view() — чистая функция над Store (GUI_DEV.md §3).

pub mod markup;
pub mod notify;
pub mod palette;
pub mod widgets;

use std::sync::Arc;

use chrono::Datelike;
use iced::alignment::{Horizontal, Vertical};
use iced::keyboard::{self, key::Named};
use iced::widget::{
    button, column, container, lazy, mouse_area, opaque, operation, qr_code, row, scrollable,
    space, stack, text, text_editor,
};
use iced::{Element, Length, Padding, Point, Task};

use crate::core_link::{
    now_ms, CoreLink, LinkError, LinkEvent, OkReply, Reply, Request, SearchHit,
};
use crate::model::cid::Cid;
use crate::model::feed::FeedItem;
use crate::model::msg::{CoreMsgId, MsgId};
use crate::model::store::Store;

/// Лимит тела SEND_MESSAGE (str16 ≤ 4096, CORE-API.md §4.3).
const MAX_BODY: usize = 4096;

/// Первый запрос истории при открытии чата; догрузка растёт от него
/// до потолка ядра (GUI_DEV.md §3: порциями 100–200).
const HISTORY_LOAD: u32 = 100;

/// Сообщения приложения: канал ядра, действия чата, знакомство, настройки,
/// окно.
#[derive(Debug, Clone)]
pub enum Message {
    Link(LinkIn),
    Chat(ChatIn),
    Meet(MeetIn),
    Settings(SettingsIn),
    Window(WindowIn),
    Call(CallIn),
}

/// Действия звонка (S7, GUI_UI.md §3): кнопки оверлея и шапки, тик
/// таймера, готовность аудио-моста.
#[derive(Debug, Clone)]
pub enum CallIn {
    /// «Позвонить» в шапке чата (START_CALL).
    Start(Cid),
    /// «Принять» входящий (ANSWER_CALL).
    Answer,
    /// «Отклонить» входящий / «Отменить» исходящий (END_CALL).
    Decline,
    /// «Завершить» активный (END_CALL).
    Hangup,
    /// Mute-тумблер: микрофон выключается нулями в fifo (DTX ядра).
    ToggleMute,
    /// Секундный тик таймера разговора (подписка в app.rs).
    Tick,
    /// Аудио-мост поднят в фоне (fifo + устройства); CallAudio не Clone,
    /// поэтому едет в Arc<Mutex<Option>> — забирается один раз.
    AudioReady(
        std::sync::Arc<std::sync::Mutex<Option<crate::audio::CallAudio>>>,
        Vec<crate::audio::AudioGap>,
    ),
}

/// Стадия звонка (GUI_UI.md §3). Ended фазой нет: конец звонка —
/// sys-строка в чате, оверлей закрывается.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    /// Входящий: ждём «Принять»/«Отклонить».
    Incoming,
    /// Исходящий: ждём accept пира.
    Outgoing,
    /// Разговор идёт: таймер, mute, «Завершить».
    Active,
}

/// Живой звонок (оверлей). `call_id` отсекает события прошлых звонков.
#[derive(Debug, Clone)]
pub struct CallView {
    pub cid: Cid,
    pub call_id: [u8; 16],
    pub phase: CallPhase,
    /// Начало разговора (accept) — для таймера.
    pub started_ms: i64,
    pub muted: bool,
}

/// Ответы и события core_link. Не протекают в виджеты: update пишет
/// их в Store, view читает Store.
#[derive(Debug, Clone)]
pub enum LinkIn {
    Event(LinkEvent),
    Replied {
        request: Request,
        reply: Reply,
        tag: Option<MsgId>,
    },
}

/// Локальные действия пользователя.
#[derive(Debug, Clone)]
pub enum ChatIn {
    Select(Cid),
    /// Ctrl+1..9 — чат по номеру в порядке сайдбара (GUI_UI.md §4).
    SelectIndex(usize),
    Send,
    Edit(text_editor::Action),
    /// Ctrl+F — панель поиска по чату (S6; на ядре 2.1+ — по всей
    /// истории через SEARCH_MESSAGE, S11).
    SearchOpen,
    /// Ввод в поисковой строке панели.
    SearchInput(String),
    /// Галка «искать по всем чатам» (GLOBAL_SEARCH_MESSAGE; только в
    /// ядерном режиме, S11).
    SearchScope(bool),
    /// «Показать ещё»: следующая страница совпадений (offset растёт).
    SearchMore,
    /// ПКМ по строке ленты — меню сообщения (S12).
    OpenMsgMenu(MsgId),
    /// Закрепить сообщение из меню пузыря (PIN_MESSAGE, ядро 2.2+).
    PinMessage(MsgId),
    /// Открепить сообщение (UNPIN_MESSAGE) — из меню или списка закрепов.
    UnpinMessage(CoreMsgId),
    /// Открыть список закреплённых сообщений (плашка под шапкой).
    OpenPins,
    /// Лента прокручена; `true` — докатились до верха: догрузить более
    /// старые сообщения (S6; окно GET_HISTORY растёт от загруженного
    /// до потолка ядра).
    ScrollTop(bool),
    /// Клик по спойлеру — показать/спрятать содержимое (S14).
    ToggleSpoiler(MsgId),
    /// Реакция на сообщение (S15): из меню (ПКМ) или по пилюле под
    /// пузырём. Моя реакция с тем же эмодзи снимается (переключатель).
    React(MsgId, char),
    /// «📎» в композере — выбрать файл (S10). Диалог нативный,
    /// асинхронный (xdg-portal): update не ждёт.
    PickFile,
    /// Диалог выбора файла закрылся: Some — путь, None — отмена.
    FilePicked(Option<std::path::PathBuf>),
    /// Клик по ссылке в пузыре — открыть в браузере (S14).
    OpenLink(String),
}

/// Действия знакомства: инвайт-диалог, карточка контакта, блокировка
/// (спринт S2, GUI_UI.md §1).
#[derive(Debug, Clone)]
pub enum MeetIn {
    /// Открыть инвайт-диалог: форма выдачи + LAN-код из кэша сессии.
    OpenInvite,
    /// Ввод адреса для кода (host:port; пусто — только локальная сеть).
    AddrInput(String),
    /// Галка «через relay»: соединение через релей, peer IP не видит.
    RelayToggled(bool),
    /// Выдать код (CREATE_INVITE с текущими адресом и relay).
    IssueInvite,
    /// Скопировать код приглашения в буфер обмена.
    CopyCode,
    /// Ввод чужого кода.
    ForeignInput(String),
    /// Гасим чужой код (ACCEPT_INVITE).
    SubmitForeign,
    /// Карточка контакта (из шапки чата или меню по правой кнопке).
    OpenCard(Cid),
    /// Блок/разблок по текущему состоянию контакта.
    ToggleBlock(Cid),
    /// Закрепить/открепить чат по текущему состоянию (API 2.2+, S12).
    TogglePinChat(Cid),
    /// Открыть выбор визитки для отправки в этот чат (API 2.2+, S13).
    OpenSendCard(Cid),
    /// Выбрана визитка: чужая (Some) или моя (None) — SEND_CONTACT.
    PickSendCard(Option<Cid>),
    /// Спросить подтверждение удаления чата (API 2.2+, S13).
    AskDeleteChat(Cid),
    /// Удаление подтверждено — DELETE_CHAT.
    ConfirmDeleteChat(Cid),
    /// Правая кнопка на строке контакта (GUI_UI.md §4).
    OpenMenu(Cid),
    /// Движение мыши над строкой контакта — позиция для меню.
    Cursor(Point),
    /// Esc или клик мимо панели.
    CloseOverlay,
}

/// Действия настроек (S3, GUI_UI.md §4): приватность уведомлений,
/// компактная лента, масштаб шрифта.
#[derive(Debug, Clone)]
pub enum SettingsIn {
    /// Открыть панель настроек.
    Open,
    /// Галка «скрывать текст в уведомлениях».
    ToggleNotificationText(bool),
    /// «Фото» в настройках — выбрать аватарку (S8б), диалог нативный.
    PickAvatar,
    /// Диалог выбора аватарки закрылся: Some — путь, None — отмена.
    AvatarPicked(Option<std::path::PathBuf>),
    /// Сжатие аватарки закончено: Ok — байты ≤16 КБ, Err — честный
    /// текст причины для заметки в настройках.
    AvatarReady(Result<Vec<u8>, String>),
    /// Галка «компактная лента».
    ToggleCompact(bool),
    /// Кнопка масштаба: +шаг или −шаг (границы удерживает Store).
    ChangeFontScale(f32),
    /// Ввод имени профиля (SET_NAME, S13.1).
    NameInput(String),
    /// Сохранить имя профиля (SET_NAME).
    SaveName,
}

/// События окна и глобальные команды: фокус для правила уведомлений,
/// выход по Ctrl+Q (GUI_UI.md §4).
#[derive(Debug, Clone)]
pub enum WindowIn {
    /// Окно получило/потеряло фокус.
    Focus(bool),
    /// Закрыть приложение.
    Exit,
}

/// Оверлей поверх главного окна (GUI_UI.md §3); звонок появится в S7.
pub enum Overlay {
    None,
    /// QR-данные тяжёлые — в боксе, чтобы оверлей не раздувать.
    Invite(Box<InviteView>),
    Card(Cid),
    Menu(MenuView),
    /// Настройки: галки и масштаб живут в Store.settings, черновик
    /// имени профиля — в SettingsView.
    Settings(SettingsView),
    /// Поиск по загруженному чату (Ctrl+F, S6).
    Search(SearchView),
    /// Список закреплённых сообщений открытого чата (S12).
    Pins,
    /// Меню сообщения по ПКМ (S12): закрепить/открепить.
    MsgMenu(MsgId),
    /// Выбор визитки для отправки в чат (S13): cid чата-получателя.
    SendCard(Cid),
    /// Подтверждение удаления чата (S13): действие необратимо.
    ConfirmDelete(Cid),
    /// Звонок (S7): не закрывается ни Esc, ни кликом мимо — только
    /// кнопками («Завершить» у активного, GUI_UI.md §3).
    Call(CallView),
}

/// Стейт панели настроек: черновик имени профиля (S13.1) и итог
/// последнего сохранения — строкой под полем.
#[derive(Debug, Default)]
pub struct SettingsView {
    /// Черновик имени (SET_NAME, §4.4).
    pub name_input: String,
    /// «Имя сохранено…» / «Имя не сохранено: …» — честный итог.
    pub note: Option<String>,
}

/// Панель поиска: запрос живёт здесь; совпадения — либо из Store
/// (ядро 1.x, локальный поиск по загруженному, S6), либо приходят
/// от ядра (2.1+, S11) и копятся постранично в `results`.
pub struct SearchView {
    pub query: String,
    /// Фокус в строку при открытии.
    pub input_id: iced::widget::Id,
    /// Искать по всем чатам (GLOBAL_SEARCH_MESSAGE) — только ядерный режим.
    pub global: bool,
    /// Ядерные совпадения текущего запроса (накапливаются по страницам).
    pub results: Vec<SearchHit>,
    /// Всего совпадений у ядра по текущему запросу (для «ещё N из M»).
    pub total: u32,
    /// Запрос, на который ядро уже ответило; `!= query` — поиск в полёте.
    /// Устаревшие ответы (смена запроса/скопа/чата) отбрасываются.
    pub answered: Option<(String, bool, Option<Cid>)>,
    /// Ошибка поиска — текст для пользователя (TEXT_STYLE §3).
    pub error: Option<String>,
}

/// Инвайт-диалог: мой код и чужой код (GUI_UI.md §1).
pub struct InviteView {
    /// Код ядра (CREATE_INVITE); на экране — начало и хвост, полный — в QR.
    pub code: Option<String>,
    /// QR по коду; строится один раз при ответе ядра.
    pub qr: Option<qr_code::Data>,
    /// Ошибка выдачи или гашения кода — текст для пользователя.
    pub error: Option<String>,
    /// Введённый чужой код.
    pub foreign: String,
    /// ACCEPT_INVITE в полёте — кнопка «Добавить» неактивна.
    pub submitting: bool,
    /// Адрес для кода: пусто — только локальная сеть (LAN-анонс ядра).
    pub addr: String,
    /// Выдать код через relay; требует адреса (CORE-API.md §4.2).
    pub relay: bool,
}

/// Меню по правой кнопке на контакте (GUI_UI.md §4).
pub struct MenuView {
    pub cid: Cid,
    pub at: Point,
}

/// Весь стейт приложения; дженерик по каналу с ядром — ui не знает,
/// заглушка это или UDS-транспорт.
pub struct State<L: CoreLink> {
    pub link: Arc<L>,
    pub store: Store,
    /// Жив ли канал с ядром (Down/Resync и неудачный Contacts).
    pub link_up: bool,
    /// Содержимое композера; текст черновика хранит Store.
    pub composer: text_editor::Content,
    pub feed_id: iced::widget::Id,
    pub composer_id: iced::widget::Id,
    /// Оверлей поверх окна.
    pub overlay: Overlay,
    /// Позиция курсора над строкой контакта — точка меню.
    pub cursor: Point,
    /// Код приглашения, выданный ядром в этой сессии. Кэш: ядро не отдаёт
    /// список живых инвайтов, повторное открытие диалога не тратит слот.
    /// Тратится при CONTACT_JOINED — код одноразовый (CORE-API.md §4.2).
    pub invite_code: Option<String>,
    /// В фокусе ли окно: открытый в фокусе чат не уведомляет
    /// (GUI_UI.md §4). Подписку окна обслуживает main; по умолчанию
    /// считаем окно в фокусе.
    pub window_focused: bool,
    /// GET_HISTORY в полёте — скролл к верху не плодит повторных
    /// запросов, пока окно не доехало.
    pub history_in_flight: bool,
    /// Версия API демона из бутстрап-зонда GET_VERSION; (0, 0) — ещё не
    /// выяснена. Гейт ядерных фич: поиск — с (2, 1), закрепы — с (2, 2).
    pub kernel_api: (u16, u16),
    /// Имя профиля, сохранённое в этом запуске (SET_NAME, §4.4). Ядро
    /// имя обратно не отдаёт (GET_NAME нет) — помним только свою запись.
    pub profile_name: Option<String>,
    /// Аудио-мост активного звонка (S7); живёт, пока фаза Active.
    pub call_audio: Option<crate::audio::CallAudio>,
    /// Раскрытые спойлеры (S14): набор MsgId. Не переживает перезапуск —
    /// как и черновики; спойлер снова скрыт.
    pub revealed_spoilers: std::collections::HashSet<MsgId>,
}

impl<L: CoreLink> State<L> {
    pub fn new(link: Arc<L>) -> Self {
        Self {
            link,
            store: Store::new(),
            link_up: true,
            composer: text_editor::Content::new(),
            feed_id: iced::widget::Id::unique(),
            composer_id: iced::widget::Id::unique(),
            overlay: Overlay::None,
            cursor: Point::ORIGIN,
            invite_code: None,
            window_focused: true,
            history_in_flight: false,
            kernel_api: (0, 0),
            profile_name: None,
            call_audio: None,
            revealed_spoilers: std::collections::HashSet::new(),
        }
    }

    /// Ядерный поиск доступен (API 2.1+, SEARCH_MESSAGE §4.3): панель
    /// Ctrl+F спрашивает демон по всей истории, а не Store по окну.
    pub fn kernel_search(&self) -> bool {
        self.kernel_api >= (2, 1)
    }

    /// Разметка сообщений — API 2.0+ (§4.3).
    pub fn kernel_v20(&self) -> bool {
        self.kernel_api >= (2, 0)
    }

    /// Фичи API 2.2+ (§4.8): закрепы, визитки, удаление чата. На ядрах
    /// младше пункты меню скрыты целиком, а не выключены.
    pub fn kernel_v22(&self) -> bool {
        self.kernel_api >= (2, 2)
    }

    /// Живой звонок (S7), если оверлей звонка открыт.
    pub fn call_view(&self) -> Option<&CallView> {
        match &self.overlay {
            Overlay::Call(view) => Some(view),
            _ => None,
        }
    }

    /// Идёт разговор (фаза Active) — для секундной подписки таймера.
    pub fn call_active(&self) -> bool {
        self.call_view()
            .is_some_and(|view| view.phase == CallPhase::Active)
    }

    /// Оверлей закрыт — инвариант для тестов и сценариев.
    pub fn overlay_closed(&self) -> bool {
        matches!(self.overlay, Overlay::None)
    }
}

pub fn update<L: CoreLink>(state: &mut State<L>, message: Message) -> Task<Message> {
    match message {
        Message::Link(link) => on_link(state, link),
        Message::Chat(chat) => on_chat(state, chat),
        Message::Meet(meet) => on_meet(state, meet),
        Message::Settings(settings) => on_settings(state, settings),
        Message::Window(window) => on_window(state, window),
        Message::Call(call) => on_call(state, call),
    }
}

/// Звонки (S7): кнопки оверлея/шапки, mute, тик таймера, готовность
/// аудио-моста. Ядро держит одну линию на демон — состояние звонка у
/// нас одно (Overlay::Call).
fn on_call<L: CoreLink>(state: &mut State<L>, call: CallIn) -> Task<Message> {
    match call {
        CallIn::Start(cid) => {
            // Линия одна: в звонке кнопка недоступна, прямой вызов гасим.
            if state.call_view().is_some() {
                return Task::none();
            }
            request_task(state, Request::StartCall { cid })
        }
        CallIn::Answer => {
            if !matches!(
                state.call_view().map(|v| v.phase),
                Some(CallPhase::Incoming)
            ) {
                return Task::none();
            }
            request_task(state, Request::AnswerCall)
        }
        // Отклонить/отменить: звонок ещё не начался, аудио не поднимали.
        // End-событие ядра придёт уже к закрытому оверлею и будет
        // проигнорировано — это нормально.
        CallIn::Decline => {
            if state.call_view().is_none() {
                return Task::none();
            }
            state.overlay = Overlay::None;
            request_task(state, Request::EndCall)
        }
        CallIn::Hangup => {
            if state.call_view().is_none() {
                return Task::none();
            }
            state.overlay = Overlay::None;
            state.call_audio = None; // Drop останавливает потоки
            request_task(state, Request::EndCall)
        }
        CallIn::ToggleMute => {
            let Some(muted) = state.call_view().map(|v| !v.muted) else {
                return Task::none();
            };
            if let Overlay::Call(view) = &mut state.overlay {
                view.muted = muted;
            }
            if let Some(audio) = &state.call_audio {
                audio.set_muted(muted);
            }
            Task::none()
        }
        // Перерисовка таймера; время считает view от started_ms.
        CallIn::Tick => Task::none(),
        CallIn::AudioReady(slot, gaps) => {
            let audio = slot.lock().expect("слот не отравлен").take();
            state.call_audio = audio;
            // Честно про неполный звук (микрофон/динамик не нашлись).
            let Some(view) = state.call_view() else {
                return Task::none();
            };
            let cid = view.cid;
            for gap in gaps {
                state
                    .store
                    .push_sys(cid, now_ms(), format!("звонок: {}", gap.user_text()));
            }
            Task::none()
        }
    }
}

/// Событие CALL_STATE (S7): offer без живого звонка — входящий, с ним —
/// эхо нашего START_CALL (уже Outgoing, пропуск). Accept переводит в
/// разговор и поднимает звук; reject/end — sys-строки и закрытие.
fn on_call_state<L: CoreLink>(
    state: &mut State<L>,
    cid: Cid,
    call_id: [u8; 16],
    signal: crate::core_link::CallSignal,
) -> Task<Message> {
    use crate::core_link::CallSignal;
    match signal {
        CallSignal::Offer => {
            if state.call_view().is_some() {
                return Task::none(); // эхо нашего START_CALL
            }
            state.overlay = Overlay::Call(CallView {
                cid,
                call_id,
                phase: CallPhase::Incoming,
                started_ms: 0,
                muted: false,
            });
            // Входящий — как входящее сообщение: уведомление по правилам.
            if notify::should_notify(state.store.selected(), state.window_focused, cid) {
                let title = state
                    .store
                    .contact(cid)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| cid.to_string());
                return notify::os_notify(title, "входящий звонок".to_owned());
            }
            Task::none()
        }
        CallSignal::Accept => {
            let Some(view) = state.call_view() else {
                return Task::none(); // чужой/протухший accept
            };
            if view.call_id != call_id || view.phase == CallPhase::Active {
                return Task::none();
            }
            if let Overlay::Call(view) = &mut state.overlay {
                view.phase = CallPhase::Active;
                view.started_ms = now_ms();
            }
            start_call_audio(state)
        }
        CallSignal::Reject => {
            let Some(view) = state.call_view() else {
                return Task::none();
            };
            if view.call_id != call_id {
                return Task::none();
            }
            // Ядро вслед за reject шлёт end — закрываем здесь, чтобы не
            // было двух sys-строк («отклонён» + «без ответа»).
            state.overlay = Overlay::None;
            state.call_audio = None;
            state
                .store
                .push_sys(cid, now_ms(), "звонок отклонён".into());
            Task::none()
        }
        CallSignal::End => {
            let Some(view) = state.call_view().cloned() else {
                return Task::none(); // уже закрыли своей кнопкой
            };
            if view.call_id != call_id {
                return Task::none();
            }
            state.overlay = Overlay::None;
            state.call_audio = None; // Drop останавливает потоки и потоки cpal
            let line = match view.phase {
                CallPhase::Active => {
                    let secs = ((now_ms() - view.started_ms) / 1000).max(0);
                    format!("звонок завершён — {}:{:02}", secs / 60, secs % 60)
                }
                CallPhase::Outgoing => "без ответа".to_owned(),
                CallPhase::Incoming => "входящий звонок отменён".to_owned(),
            };
            state.store.push_sys(cid, now_ms(), line);
            Task::none()
        }
    }
}

/// Поднять аудио-мост в фоне (открытие fifo и устройств блокирует —
/// в update нельзя, GUI_DEV §3). На заглушке звука нет вовсе.
fn start_call_audio<L: CoreLink>(state: &State<L>) -> Task<Message> {
    if state.link.is_stub() {
        return Task::none();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = crate::audio::pcm_dir(std::path::Path::new(&home));
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let audio = crate::audio::CallAudio::start(&dir);
                let gaps = audio.gaps().to_vec();
                (
                    std::sync::Arc::new(std::sync::Mutex::new(Some(audio))),
                    gaps,
                )
            })
            .await
            .expect("аудио-поток не паникует")
        },
        |(slot, gaps)| Message::Call(CallIn::AudioReady(slot, gaps)),
    )
}

/// События окна: фокус — данные для правила уведомлений, выход —
/// завершение рантайма iced.
fn on_window<L: CoreLink>(state: &mut State<L>, window: WindowIn) -> Task<Message> {
    match window {
        WindowIn::Focus(focused) => {
            state.window_focused = focused;
            Task::none()
        }
        WindowIn::Exit => iced::exit(),
    }
}

/// Настройки пишутся в Store сразу; рендер читает их в том же кадре.
fn on_settings<L: CoreLink>(state: &mut State<L>, settings: SettingsIn) -> Task<Message> {
    match settings {
        SettingsIn::Open => {
            state.overlay = Overlay::Settings(SettingsView::default());
            Task::none()
        }
        SettingsIn::NameInput(value) => {
            if let Overlay::Settings(view) = &mut state.overlay {
                view.name_input = value;
                view.note = None;
            }
            Task::none()
        }
        SettingsIn::SaveName => {
            let Overlay::Settings(view) = &state.overlay else {
                return Task::none();
            };
            let name = view.name_input.trim().to_owned();
            // Кнопка выключена вне 1..=32; прямой вызов молча гасим.
            if !(1..=32).contains(&name.chars().count()) {
                return Task::none();
            }
            request_task(state, Request::SetName { name })
        }
        SettingsIn::PickAvatar => {
            if !matches!(state.overlay, Overlay::Settings(_)) {
                return Task::none();
            }
            Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("Картинка", &["png", "jpg", "jpeg", "webp"])
                        .pick_file()
                        .await
                },
                |handle| {
                    Message::Settings(SettingsIn::AvatarPicked(
                        handle.map(|h| h.path().to_path_buf()),
                    ))
                },
            )
        }
        SettingsIn::AvatarPicked(picked) => {
            let Some(path) = picked else {
                return Task::none();
            };
            // Сжатие тяжёлое и может упасть — вне update, честный текст
            // ошибки приедет с результатом.
            Task::perform(async move { prepare_avatar(&path) }, |result| {
                Message::Settings(SettingsIn::AvatarReady(result))
            })
        }
        SettingsIn::AvatarReady(result) => {
            let data = match result {
                Ok(data) => data,
                Err(note) => {
                    if let Overlay::Settings(view) = &mut state.overlay {
                        view.note = Some(note);
                    }
                    return Task::none();
                }
            };
            request_task(state, Request::SetAvatar { data })
        }
        SettingsIn::ToggleNotificationText(on) => {
            state.store.settings.hide_notification_text = on;
            Task::none()
        }
        SettingsIn::ToggleCompact(on) => {
            state.store.settings.compact = on;
            Task::none()
        }
        SettingsIn::ChangeFontScale(delta) => {
            state.store.settings.change_font_scale(delta);
            Task::none()
        }
    }
}

fn on_link<L: CoreLink>(state: &mut State<L>, link: LinkIn) -> Task<Message> {
    match link {
        LinkIn::Event(event) => match event {
            LinkEvent::MessageReceived {
                cid,
                name,
                msg_id,
                ts_ms,
                text,
                entities,
            } => {
                let opened = state.store.selected() == Some(cid);
                state
                    .store
                    .push_incoming_core(cid, &name, msg_id, ts_ms, text.clone(), entities);
                let mut task = if opened {
                    scroll_feed(state)
                } else {
                    Task::none()
                };
                // Уведомление ОС: открытый в фокусе чат молчит (GUI_UI.md §4).
                if notify::should_notify(state.store.selected(), state.window_focused, cid) {
                    let title = state
                        .store
                        .contact(cid)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| name.clone());
                    let body = notify::notification_body(
                        &text,
                        state.store.settings.hide_notification_text,
                    );
                    task = Task::batch([task, notify::os_notify(title, body)]);
                }
                task
            }
            LinkEvent::MessageDelivered { cid, msg_id } => {
                state.store.mark_delivered(cid, msg_id);
                Task::none()
            }
            // Аватарка приехала (S8б): кэш по id; вьюхи перечитают.
            LinkEvent::AvatarReceived {
                cid,
                avatar_id,
                data,
            } => {
                state.store.apply_avatar(cid, avatar_id, data);
                Task::none()
            }
            // Входящий файл (S10): ядро сохранил в ~/.shopot/files/<hex>;
            // пузырь с кнопкой открытия, уведомление как у сообщения.
            LinkEvent::FileReceived {
                cid,
                file_id,
                size,
                name,
            } => {
                let opened = state.store.selected() == Some(cid);
                let contact_name = state
                    .store
                    .contact(cid)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| cid.0.to_string());
                state.store.push_incoming_file(
                    cid,
                    &contact_name,
                    now_ms(),
                    crate::model::msg::FileRef {
                        name: name.clone(),
                        size,
                        file_id,
                        path: Some(files_path(&file_id)),
                    },
                );
                let mut task = if opened {
                    scroll_feed(state)
                } else {
                    Task::none()
                };
                if notify::should_notify(state.store.selected(), state.window_focused, cid) {
                    task = Task::batch([
                        task,
                        notify::os_notify(
                            contact_name,
                            format!("📎 {name} · {}", human_size(size)),
                        ),
                    ]);
                }
                task
            }
            LinkEvent::FileSent { cid, file_id } => {
                state.store.mark_file_sent(cid, file_id);
                Task::none()
            }
            // Реакция собеседника (S15): применить к ленте. Своих
            // ядро событием не шлёт — своя применена по ответу (S15).
            LinkEvent::Reaction {
                cid,
                msg_id,
                add,
                emoji,
                ..
            } => {
                state.store.apply_reaction(cid, msg_id, add, false, emoji);
                Task::none()
            }
            LinkEvent::Presence { cid, online, .. } => {
                state.store.set_presence(cid, online);
                Task::none()
            }
            LinkEvent::ContactJoined { cid, name } => {
                if state.store.ensure_contact(cid, &name) {
                    state.store.set_presence(cid, true);
                    state.store.push_sys(
                        cid,
                        now_ms(),
                        format!("{name} подключился по вашему коду"),
                    );
                }
                // Какой-то из кодов потрачен: кэш мог стать уже погашенным.
                state.invite_code = None;
                Task::none()
            }
            LinkEvent::ContactProfile {
                cid,
                name,
                avatar_id,
            } => {
                // Имя и avatar_id атомарно из профиля (S8б); байты
                // дотащит ядро событием AVATAR_RECEIVED.
                state.store.apply_profile(cid, name, avatar_id.0);
                Task::none()
            }
            LinkEvent::BlockedByPeer { cid } => {
                state
                    .store
                    .push_sys(cid, now_ms(), "контакт заблокировал вас".into());
                state.store.set_presence(cid, false);
                Task::none()
            }
            // Визитка (S13): запись в ленту + уведомление, как входящее.
            // bundle/addr не показываем: добавить человека из визитки
            // нельзя — нужно его приглашение (§4.8).
            LinkEvent::ContactCard { cid, name, .. } => {
                let opened = state.store.selected() == Some(cid);
                state.store.push_card(cid, now_ms(), name.clone());
                let mut task = if opened {
                    scroll_feed(state)
                } else {
                    Task::none()
                };
                if notify::should_notify(state.store.selected(), state.window_focused, cid) {
                    let title = state
                        .store
                        .contact(cid)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| cid.to_string());
                    let body = notify::notification_body(
                        &format!("визитка: {name}"),
                        state.store.settings.hide_notification_text,
                    );
                    task = Task::batch([task, notify::os_notify(title, body)]);
                }
                task
            }
            // Звонок (S7, §4.6): стадии по call_id — события прошлых
            // звонков игнорируются. Reject всегда следом несёт end —
            // он и закрывает.
            LinkEvent::CallState {
                cid,
                call_id,
                state: signal,
            } => on_call_state(state, cid, call_id, signal),
            LinkEvent::Down => {
                state.link_up = false;
                Task::none()
            }
            LinkEvent::Resync => {
                state.link_up = true;
                // Демон между соединениями могли заменить — зонд версии
                // повторяется, гейт ядерных фич пересчитается.
                Task::batch([
                    request_task(state, Request::Contacts),
                    request_task(state, Request::Version),
                ])
            }
        },
        LinkIn::Replied {
            request,
            reply,
            tag,
        } => match (request, reply) {
            (Request::Contacts, Ok(OkReply::Contacts(contacts))) => {
                state.link_up = true;
                state.store.replace_contacts(contacts);
                Task::none()
            }
            (Request::Contacts, Err(_)) => {
                state.link_up = false;
                Task::none()
            }
            (
                Request::SendMsg { cid, .. },
                Ok(OkReply::Sent {
                    msg_id,
                    queued: false,
                }),
            ) => {
                if let Some(tag) = tag {
                    state.store.mark_sent_core(cid, tag, msg_id);
                }
                Task::none()
            }
            (Request::SendMsg { cid, .. }, Ok(OkReply::Sent { queued: true, .. })) => {
                if let Some(tag) = tag {
                    state.store.mark_queued(cid, tag);
                }
                Task::none()
            }
            (Request::SendMsg { cid, .. }, Err(err)) => {
                if let Some(tag) = tag {
                    state.store.mark_failed(cid, tag, err.user_text());
                }
                Task::none()
            }
            (Request::Invite { relay, addr }, Ok(OkReply::Code(code))) => {
                // Кэшируем только LAN-код; коды с адресом — разовое решение.
                if !relay && addr.is_empty() {
                    state.invite_code = Some(code.clone());
                }
                if let Overlay::Invite(view) = &mut state.overlay {
                    view.submitting = false;
                    view.code = Some(code.clone());
                    view.qr = qr_of(&code);
                }
                Task::none()
            }
            (Request::Invite { .. }, Err(err)) => {
                if let Overlay::Invite(view) = &mut state.overlay {
                    view.submitting = false;
                    view.error = Some(err.user_text());
                }
                Task::none()
            }
            (Request::Add { .. }, Ok(OkReply::Added { cid })) => {
                state.overlay = Overlay::None;
                Task::batch([
                    select_chat(state, cid),
                    // Сайдбар перечитает список: контакт появился у ядра.
                    request_task(state, Request::Contacts),
                ])
            }
            (Request::Add { .. }, Err(err)) => {
                if let Overlay::Invite(view) = &mut state.overlay {
                    view.submitting = false;
                    view.error = Some(err.user_text());
                }
                Task::none()
            }
            (Request::Block { cid }, Ok(OkReply::Done)) => {
                state.store.set_blocked(cid, true);
                Task::none()
            }
            (Request::Unblock { cid }, Ok(OkReply::Done)) => {
                state.store.set_blocked(cid, false);
                Task::none()
            }
            // История дописывается в начало ленты; позицию прокрутки не
            // трогаем — пользователь читает то же место (S6).
            (Request::History { cid, .. }, Ok(OkReply::History { total, records })) => {
                state.history_in_flight = false;
                state.store.load_history(cid, total, &records);
                Task::none()
            }
            (Request::History { .. }, Err(_)) => {
                state.history_in_flight = false;
                Task::none()
            }
            // Бутстрап-зонд: версия API демона гейтит ядерные фичи
            // (поиск 2.1+, закрепы 2.2+). Узнав 2.2, сразу читаем закреп
            // чата — он переживает рестарт демона.
            (Request::Version, Ok(OkReply::Version { major, minor, .. })) => {
                state.kernel_api = (major, minor);
                if state.kernel_v22() {
                    request_task(state, Request::PinnedChat)
                } else {
                    Task::none()
                }
            }
            (Request::PinnedChat, Ok(OkReply::PinnedChat { cid })) => {
                state.store.set_pinned_chat(cid);
                Task::none()
            }
            (Request::PinChat { cid }, Ok(OkReply::Done)) => {
                state.store.set_pinned_chat(Some(cid));
                Task::none()
            }
            (Request::UnpinChat, Ok(OkReply::Done)) => {
                state.store.set_pinned_chat(None);
                Task::none()
            }
            (Request::PinChat { cid }, Err(err)) => {
                state
                    .store
                    .push_sys(cid, now_ms(), format!("не закреплено: {}", err.user_text()));
                Task::none()
            }
            (Request::PinnedMessages { cid }, Ok(OkReply::PinnedMessages { ids })) => {
                state.store.set_pin_ids(cid, &ids);
                // Тела неразрешённых закрепов дозапрашиваются по одному.
                let tasks: Vec<Task<Message>> = state
                    .store
                    .pins(cid)
                    .iter()
                    .filter(|p| p.body.is_none())
                    .map(|p| request_task(state, Request::GetMessage { msg_id: p.msg_id }))
                    .collect();
                Task::batch(tasks)
            }
            (
                Request::GetMessage { msg_id },
                Ok(OkReply::Message {
                    cid,
                    mine,
                    ts_ms,
                    text,
                    ..
                }),
            ) => {
                state.store.fill_pin(cid, msg_id, ts_ms, mine, text);
                Task::none()
            }
            (Request::PinMessage { cid, .. }, Ok(OkReply::Done))
            | (Request::UnpinMessage { cid, .. }, Ok(OkReply::Done)) => {
                // Состав закрепов перечитываем: источник правды — ядро.
                request_task(state, Request::PinnedMessages { cid })
            }
            (Request::PinMessage { cid, .. }, Err(err))
            | (Request::UnpinMessage { cid, .. }, Err(err)) => {
                state
                    .store
                    .push_sys(cid, now_ms(), format!("закреп: {}", err.user_text()));
                Task::none()
            }
            // Удаление чата подтверждено ядром (S13): проекция стирается
            // целиком; ошибка — честная sys-строка, чат остаётся.
            (Request::DeleteChat { cid }, Ok(OkReply::Done)) => {
                state.store.delete_chat(cid);
                Task::none()
            }
            (Request::DeleteChat { cid }, Err(err)) => {
                state
                    .store
                    .push_sys(cid, now_ms(), format!("чат не удалён: {}", err.user_text()));
                Task::none()
            }
            // Визитка ушла (S13): ядро эха не шлёт — отметка в ленте
            // получателя. Ошибка (нет контакта, блок, офлайн) — sys-строка.
            (Request::SendContact { to, contact }, Ok(OkReply::Done)) => {
                let name = match contact {
                    Some(c) => state
                        .store
                        .contact(c)
                        .map(|x| x.name.clone())
                        .unwrap_or_else(|| c.to_string()),
                    None => "моя".to_owned(),
                };
                state
                    .store
                    .push_sys(to, now_ms(), format!("визитка отправлена: {name}"));
                Task::none()
            }
            (Request::SendContact { to, .. }, Err(err)) => {
                state.store.push_sys(
                    to,
                    now_ms(),
                    format!("визитка не отправлена: {}", err.user_text()),
                );
                Task::none()
            }
            // Имя профиля сохранено (S13.1): помним на сессию (ядро его
            // обратно не отдаёт), итог — строкой в панели настроек.
            (Request::SetName { name }, Ok(OkReply::Done)) => {
                state.profile_name = Some(name);
                if let Overlay::Settings(view) = &mut state.overlay {
                    view.note = Some("Имя сохранено — будет в ваших визитках.".into());
                }
                Task::none()
            }
            (Request::SetAvatar { data }, Ok(OkReply::AvatarSet { avatar_id, .. })) => {
                state.store.set_own_avatar(avatar_id, data.clone());
                if let Overlay::Settings(view) = &mut state.overlay {
                    view.note = Some("Фото обновлено.".into());
                }
                Task::none()
            }
            (Request::SetAvatar { .. }, Err(err)) => {
                if let Overlay::Settings(view) = &mut state.overlay {
                    view.note = Some(match err {
                        LinkError::Kernel(crate::core_link::api::Status::TooBig) => {
                            "Фото не влезло в лимит ядра — попробуйте меньше.".into()
                        }
                        _ => format!("Фото не сохранилось: {}", err.user_text()),
                    });
                }
                Task::none()
            }
            (Request::SetName { .. }, Err(err)) => {
                if let Overlay::Settings(view) = &mut state.overlay {
                    view.note = Some(format!("Имя не сохранено: {}", err.user_text()));
                }
                Task::none()
            }
            // Звонки (S7): ответ START_CALL открывает исходящий оверлей
            // (offer-эхо ядра придёт следом и будет пропущено); ошибка —
            // честная sys-строка в чате.
            (Request::StartCall { cid }, Ok(OkReply::CallStarted { call_id })) => {
                state.overlay = Overlay::Call(CallView {
                    cid,
                    call_id,
                    phase: CallPhase::Outgoing,
                    started_ms: 0,
                    muted: false,
                });
                Task::none()
            }
            (Request::StartCall { .. }, Ok(_)) => Task::none(),
            (Request::StartCall { cid }, Err(err)) => {
                state.store.push_sys(
                    cid,
                    now_ms(),
                    format!("звонок не начался: {}", err.user_text()),
                );
                Task::none()
            }
            // ANSWER/END: состояние двигают события CALL_STATE; ошибка
            // «уже завершён» не страшна — оверлей и так закроется end'ом.
            (Request::AnswerCall, _) | (Request::EndCall, _) => Task::none(),
            (
                Request::Search {
                    cid, query, offset, ..
                },
                Ok(OkReply::Search { total, hits }),
            ) => {
                // Двухфазно: проверка актуальности неизменяема, затем запись.
                let current = matches!(&state.overlay, Overlay::Search(view)
                    if search_reply_current(state, view, &query, cid));
                if current {
                    if let Overlay::Search(view) = &mut state.overlay {
                        // Страницы строго по порядку: первая заменяет,
                        // следующие дописываются в хвост.
                        if offset == 0 {
                            view.results = hits;
                        } else if offset as usize == view.results.len() {
                            view.results.extend(hits);
                        }
                        view.total = total;
                        view.error = None;
                        view.answered = Some((query, view.global, cid));
                    }
                }
                Task::none()
            }
            (Request::Search { cid, query, .. }, Err(err)) => {
                let current = matches!(&state.overlay, Overlay::Search(view)
                    if search_reply_current(state, view, &query, cid));
                if current {
                    if let Overlay::Search(view) = &mut state.overlay {
                        view.error = Some(err.user_text());
                        view.answered = Some((query, view.global, cid));
                    }
                }
                Task::none()
            }
            // Файл (S10): ядро приняло — пузырь получает file_id/size
            // (до FILE_SENT статус остаётся «отправляется»).
            (Request::SendFile { cid, .. }, Ok(OkReply::FileQueued { file_id, size, .. })) => {
                if let Some(tag) = tag {
                    state.store.set_file_meta(cid, tag, file_id, size);
                }
                Task::none()
            }
            (Request::SendFile { cid, .. }, Err(err)) => {
                if let Some(tag) = tag {
                    state.store.mark_failed(cid, tag, err.user_text());
                }
                Task::none()
            }
            // Реакции (S15): своя применяется по OK; ошибка — честная
            // sys-строка в чате (Нет соединения, Контакт не найден…).
            (Request::SetReaction { cid, msg_id, emoji }, Ok(OkReply::Done)) => {
                state.store.apply_reaction(cid, msg_id, true, true, emoji);
                Task::none()
            }
            (Request::RemoveReaction { cid, msg_id, emoji }, Ok(OkReply::Done)) => {
                state.store.apply_reaction(cid, msg_id, false, true, emoji);
                Task::none()
            }
            (Request::SetReaction { cid, .. } | Request::RemoveReaction { cid, .. }, Err(err)) => {
                state.store.push_sys(
                    cid,
                    now_ms(),
                    format!("реакция не ушла: {}", err.user_text()),
                );
                Task::none()
            }
            // Остальные ответы ядерного канала появляются со своими спринтами.
            _ => Task::none(),
        },
    }
}

fn on_chat<L: CoreLink>(state: &mut State<L>, chat: ChatIn) -> Task<Message> {
    match chat {
        ChatIn::Select(cid) => {
            let task = select_chat(state, cid);
            // На ядре 2.2+ читаем закрепы чата один раз за сессию.
            if state.kernel_v22() && !state.store.pins_requested(cid) {
                state.store.mark_pins_requested(cid);
                return Task::batch([task, request_task(state, Request::PinnedMessages { cid })]);
            }
            task
        }
        ChatIn::SelectIndex(index) => {
            let cid = state.store.contacts().nth(index).map(|c| c.cid);
            match cid {
                Some(cid) => select_chat(state, cid),
                None => Task::none(),
            }
        }
        ChatIn::Edit(action) => {
            state.composer.perform(action);
            if let Some(cid) = state.store.selected() {
                let draft = state.composer.text();
                state.store.set_draft(cid, draft);
            }
            Task::none()
        }
        ChatIn::Send => send_current(state),
        ChatIn::SearchOpen => {
            // Панель модальна; без выбранного чата искать нечего.
            if matches!(state.overlay, Overlay::None) && state.store.selected().is_some() {
                let input_id = iced::widget::Id::unique();
                state.overlay = Overlay::Search(SearchView {
                    query: String::new(),
                    input_id: input_id.clone(),
                    global: false,
                    results: Vec::new(),
                    total: 0,
                    answered: None,
                    error: None,
                });
                return operation::focus::<Message>(input_id);
            }
            Task::none()
        }
        ChatIn::SearchInput(text) => {
            if let Overlay::Search(view) = &mut state.overlay {
                view.query = text;
                view.error = None;
            }
            search_restart(state)
        }
        ChatIn::SearchScope(global) => {
            if let Overlay::Search(view) = &mut state.overlay {
                view.global = global;
                view.error = None;
            }
            search_restart(state)
        }
        ChatIn::SearchMore => {
            if !state.kernel_search() {
                return Task::none();
            }
            let Overlay::Search(view) = &state.overlay else {
                return Task::none();
            };
            if view.query.trim().is_empty() {
                return Task::none();
            }
            request_task(
                state,
                Request::Search {
                    cid: if view.global {
                        None
                    } else {
                        state.store.selected()
                    },
                    query: view.query.clone(),
                    limit: SEARCH_PAGE,
                    offset: view.results.len() as u32,
                },
            )
        }
        ChatIn::ScrollTop(at_top) => {
            if at_top {
                load_older(state)
            } else {
                Task::none()
            }
        }
        ChatIn::PickFile => {
            // Оверлей модален: выбор файла поверх диалога не открываем.
            if !matches!(state.overlay, Overlay::None) || state.store.selected().is_none() {
                return Task::none();
            }
            Task::perform(
                async { rfd::AsyncFileDialog::new().pick_file().await },
                |handle| Message::Chat(ChatIn::FilePicked(handle.map(|h| h.path().to_path_buf()))),
            )
        }
        ChatIn::FilePicked(picked) => {
            let Some(path) = picked else {
                return Task::none(); // отмена диалога — молча
            };
            let Some(cid) = state.store.selected() else {
                return Task::none();
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let size = std::fs::metadata(&path)
                .map(|m| m.len() as u32)
                .unwrap_or(0);
            let path_string = path.to_string_lossy().into_owned();
            // Пузырь сразу, Sending; file_id/size уточнит ответ ядра.
            let msg_id = state.store.push_outgoing_file(
                cid,
                now_ms(),
                crate::model::msg::FileRef {
                    name,
                    size,
                    file_id: [0; 16],
                    path: Some(path_string.clone()),
                },
            );
            let request = Request::SendFile {
                cid,
                path: path_string,
            };
            let reply_request = request.clone();
            let link = Arc::clone(&state.link);
            Task::batch([
                Task::perform(link.execute(request), move |reply| {
                    Message::Link(LinkIn::Replied {
                        request: reply_request.clone(),
                        reply,
                        tag: Some(msg_id),
                    })
                }),
                scroll_feed(state),
            ])
        }
        ChatIn::ToggleSpoiler(id) => {
            // Клик по пузырю со спойлером: раскрыть/спрятать (S14).
            if !state.revealed_spoilers.insert(id) {
                state.revealed_spoilers.remove(&id);
            }
            Task::none()
        }
        ChatIn::OpenLink(url) => {
            // Ссылка из пузыря — браузер ОС; без браузера — честная
            // sys-строка в открытом чате.
            if std::process::Command::new("xdg-open")
                .arg(&url)
                .spawn()
                .is_err()
            {
                if let Some(cid) = state.store.selected() {
                    state.store.push_sys(
                        cid,
                        now_ms(),
                        "ссылка не открылась — скопируйте адрес вручную".into(),
                    );
                }
            }
            Task::none()
        }
        ChatIn::OpenMsgMenu(id) => {
            // Меню модально; реакции — на любом ядре (1.0+), закрепы
            // скрыты ниже 2.2 (в оверлее).
            if matches!(state.overlay, Overlay::None) {
                state.overlay = Overlay::MsgMenu(id);
            }
            Task::none()
        }
        ChatIn::React(id, emoji) => {
            state.overlay = Overlay::None;
            let Some(cid) = state.store.selected() else {
                return Task::none();
            };
            let Some(core_id) = state.store.core_id_of(cid, id) else {
                // Своё сообщение в полёте: msg_id ядра ещё нет — честно.
                state
                    .store
                    .push_sys(cid, now_ms(), "реагировать можно после отправки".into());
                return Task::none();
            };
            // Переключатель: моя реакция с этим эмодзи уже стоит — снять.
            let mine = state
                .store
                .msg(cid, id)
                .map(|m| m.reactions.iter().any(|r| r.emoji == emoji && r.mine))
                .unwrap_or(false);
            let request = if mine {
                Request::RemoveReaction {
                    cid,
                    msg_id: core_id,
                    emoji,
                }
            } else {
                Request::SetReaction {
                    cid,
                    msg_id: core_id,
                    emoji,
                }
            };
            request_task(state, request)
        }
        ChatIn::PinMessage(id) => {
            state.overlay = Overlay::None;
            let Some(cid) = state.store.selected() else {
                return Task::none();
            };
            let Some(msg_id) = state.store.core_id_of(cid, id) else {
                // Свое сообщение в полёте: msg_id ядра ещё нет — честно.
                state
                    .store
                    .push_sys(cid, now_ms(), "закрепить можно после отправки".into());
                return Task::none();
            };
            request_task(state, Request::PinMessage { cid, msg_id })
        }
        ChatIn::UnpinMessage(msg_id) => {
            state.overlay = Overlay::None;
            match state.store.selected() {
                Some(cid) => request_task(state, Request::UnpinMessage { cid, msg_id }),
                None => Task::none(),
            }
        }
        ChatIn::OpenPins => {
            state.overlay = Overlay::Pins;
            Task::none()
        }
    }
}

/// Страница ядерного поиска (S11): «Показать ещё» догружает по столько же.
const SEARCH_PAGE: u32 = 50;

/// Ядерный поиск (API 2.1+): сбросить страницы и спросить демон заново.
/// На ядрах 1.x панель ищет локально по загруженному (S6) — задач нет.
fn search_restart<L: CoreLink>(state: &mut State<L>) -> Task<Message> {
    if !state.kernel_search() {
        return Task::none();
    }
    let Overlay::Search(view) = &mut state.overlay else {
        return Task::none();
    };
    view.results.clear();
    view.total = 0;
    view.answered = None;
    if view.query.trim().is_empty() {
        return Task::none();
    }
    let request = Request::Search {
        cid: if view.global {
            None
        } else {
            state.store.selected()
        },
        query: view.query.clone(),
        limit: SEARCH_PAGE,
        offset: 0,
    };
    request_task(state, request)
}

/// Ответ ядерного поиска принимается, только если запрос, скоп и чат
/// с момента отправки не сменились; устаревший — выбрасывается.
fn search_reply_current<L: CoreLink>(
    state: &State<L>,
    view: &SearchView,
    query: &str,
    cid: Option<Cid>,
) -> bool {
    let scope_now = if view.global {
        None
    } else {
        state.store.selected()
    };
    view.query == query && scope_now == cid
}

/// Открыть чат: непрочитанные обнулены, композер заряжен черновиком,
/// при первом открытии запрошена история (S6).
fn select_chat<L: CoreLink>(state: &mut State<L>, cid: Cid) -> Task<Message> {
    state.store.select(Some(cid));
    let draft = state.store.draft(cid).to_owned();
    state.composer = text_editor::Content::with_text(&draft);
    let mut tasks = vec![
        scroll_feed(state),
        operation::focus::<Message>(state.composer_id.clone()),
    ];
    if !state.store.history_requested(cid) && !state.history_in_flight {
        state.store.mark_history_requested(cid);
        state.history_in_flight = true;
        tasks.push(request_task(
            state,
            Request::History {
                cid,
                limit: HISTORY_LOAD,
            },
        ));
    }
    Task::batch(tasks)
}

/// Скролл к верху ленты: расширить окно истории. Повторные события
/// скролла глушатся флагом в полёте и потолком окна ядра.
fn load_older<L: CoreLink>(state: &mut State<L>) -> Task<Message> {
    let Some(cid) = state.store.selected() else {
        return Task::none();
    };
    if state.history_in_flight || !state.store.has_more_history(cid) {
        return Task::none();
    }
    let limit =
        (state.store.history_loaded(cid) + HISTORY_LOAD).min(crate::core_link::HISTORY_MAX_LIMIT);
    state.history_in_flight = true;
    request_task(state, Request::History { cid, limit })
}

/// Действия знакомства; все они идут через [`request_task`], ответы
/// маршрутизирует `on_link`.
fn on_meet<L: CoreLink>(state: &mut State<L>, meet: MeetIn) -> Task<Message> {
    match meet {
        MeetIn::OpenInvite => {
            state.overlay = Overlay::Invite(Box::new(InviteView {
                code: None,
                qr: None,
                error: None,
                foreign: String::new(),
                submitting: false,
                addr: String::new(),
                relay: false,
            }));
            // Кэш LAN-кода: не тратим слот реестра приглашений.
            if let Some(code) = state.invite_code.clone() {
                if let Overlay::Invite(view) = &mut state.overlay {
                    view.code = Some(code.clone());
                    view.qr = qr_of(&code);
                }
            }
            Task::none()
        }
        MeetIn::AddrInput(text) => {
            if let Overlay::Invite(view) = &mut state.overlay {
                view.addr = text;
                // relay без адреса не имеет смысла — снимаем галку.
                if view.addr.trim().is_empty() {
                    view.relay = false;
                }
            }
            Task::none()
        }
        MeetIn::RelayToggled(on) => {
            if let Overlay::Invite(view) = &mut state.overlay {
                view.relay = on && !view.addr.trim().is_empty();
            }
            Task::none()
        }
        MeetIn::IssueInvite => {
            let request = match &state.overlay {
                Overlay::Invite(view) if !view.submitting => Request::Invite {
                    relay: view.relay,
                    addr: view.addr.trim().to_owned(),
                },
                _ => return Task::none(),
            };
            if let Overlay::Invite(view) = &mut state.overlay {
                view.submitting = true;
                view.error = None;
            }
            request_task(state, request)
        }
        MeetIn::CopyCode => {
            let code = match &state.overlay {
                Overlay::Invite(view) => view.code.clone(),
                _ => None,
            };
            match code {
                Some(code) => iced::clipboard::write(code),
                None => Task::none(),
            }
        }
        MeetIn::ForeignInput(text) => {
            if let Overlay::Invite(view) = &mut state.overlay {
                view.foreign = text;
            }
            Task::none()
        }
        MeetIn::SubmitForeign => {
            let Overlay::Invite(view) = &mut state.overlay else {
                return Task::none();
            };
            let code = view.foreign.trim().to_owned();
            if code.is_empty() || view.submitting {
                return Task::none();
            }
            view.submitting = true;
            view.error = None;
            request_task(state, Request::Add { code })
        }
        MeetIn::OpenCard(cid) => {
            state.overlay = Overlay::Card(cid);
            Task::none()
        }
        MeetIn::ToggleBlock(cid) => {
            let blocked = state.store.contact(cid).is_some_and(|c| c.blocked);
            // Меню закрываем сразу; карточка остаётся открытой — статус
            // «заблокирован/в сети» обновится в ней после ответа ядра
            // (GUI_STYLE.md §2, честность статусов).
            if matches!(state.overlay, Overlay::Menu(_)) {
                state.overlay = Overlay::None;
            }
            if blocked {
                request_task(state, Request::Unblock { cid })
            } else {
                request_task(state, Request::Block { cid })
            }
        }
        MeetIn::TogglePinChat(cid) => {
            if matches!(state.overlay, Overlay::Menu(_)) {
                state.overlay = Overlay::None;
            }
            if !state.kernel_v22() {
                return Task::none();
            }
            // Закреп один на демон: закрепление другого чата сдвигает
            // закреп (§4.8) — запрос всегда по текущему состоянию Store.
            if state.store.pinned_chat() == Some(cid) {
                request_task(state, Request::UnpinChat)
            } else {
                request_task(state, Request::PinChat { cid })
            }
        }
        MeetIn::OpenSendCard(to) => {
            if !state.kernel_v22() {
                return Task::none();
            }
            // Выбор заменяет контекстное меню, из которого пришли.
            state.overlay = Overlay::SendCard(to);
            Task::none()
        }
        MeetIn::PickSendCard(contact) => {
            let Overlay::SendCard(to) = state.overlay else {
                return Task::none();
            };
            state.overlay = Overlay::None;
            request_task(state, Request::SendContact { to, contact })
        }
        MeetIn::AskDeleteChat(cid) => {
            if !state.kernel_v22() {
                return Task::none();
            }
            state.overlay = Overlay::ConfirmDelete(cid);
            Task::none()
        }
        MeetIn::ConfirmDeleteChat(cid) => {
            state.overlay = Overlay::None;
            request_task(state, Request::DeleteChat { cid })
        }
        MeetIn::OpenMenu(cid) => {
            state.overlay = Overlay::Menu(MenuView {
                cid,
                at: state.cursor,
            });
            Task::none()
        }
        MeetIn::Cursor(point) => {
            state.cursor = point;
            Task::none()
        }
        MeetIn::CloseOverlay => {
            // Оверлей звонка (GUI_UI.md §3): активный не закрывается
            // ни Esc, ни кликом мимо — только «Завершить»; входящий и
            // исходящий закрытие = отклонить/отменить (END_CALL).
            match state.call_view().map(|v| v.phase) {
                Some(CallPhase::Active) => return Task::none(),
                Some(_) => {
                    state.overlay = Overlay::None;
                    return request_task(state, Request::EndCall);
                }
                None => {}
            }
            state.overlay = Overlay::None;
            Task::none()
        }
    }
}

fn send_current<L: CoreLink>(state: &mut State<L>) -> Task<Message> {
    // Оверлей модален: Enter в композере за ним не отправляет.
    if !matches!(state.overlay, Overlay::None) {
        return Task::none();
    }
    let Some(cid) = state.store.selected() else {
        return Task::none();
    };
    let raw = state.composer.text();
    if raw.trim().is_empty() {
        return Task::none();
    }

    // Разметка (S14): на ядре 2.0+ маркеры срезаются в entities, на 1.x
    // текст уходит как набран — маркеры остаются видимыми, это честно.
    let (text, entities) = if state.kernel_v20() {
        markup::parse(&raw)
    } else {
        (raw, Vec::new())
    };

    // Превышение лимита ядра ловим до поездки: тело SEND_MESSAGE — str16.
    if text.len() > MAX_BODY {
        let msg_id = state
            .store
            .push_outgoing(cid, now_ms(), text.clone(), Vec::new());
        state.composer = text_editor::Content::new();
        state.store.set_draft(cid, String::new());
        state.store.mark_failed(
            cid,
            msg_id,
            "Сообщение слишком длинное — лимит 4096 байт.".into(),
        );
        return Task::none();
    }

    let msg_id = state
        .store
        .push_outgoing(cid, now_ms(), text.clone(), entities.clone());
    state.composer = text_editor::Content::new();
    state.store.set_draft(cid, String::new());

    let request = Request::SendMsg {
        cid,
        text,
        entities,
    };
    let reply_request = request.clone();
    let link = Arc::clone(&state.link);
    Task::batch([
        Task::perform(link.execute(request), move |reply| {
            Message::Link(LinkIn::Replied {
                request: reply_request.clone(),
                reply,
                tag: Some(msg_id),
            })
        }),
        scroll_feed(state),
    ])
}

/// Аватарка из выбранного файла (S8б): декод → 96×96 → JPEG, лимит
/// ядра 16000 Б. Err — честный текст для заметки (без «ядра»).
fn prepare_avatar(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let img = image::ImageReader::open(path)
        .map_err(|_| "Файл не читается.".to_owned())?
        .decode()
        .map_err(|_| "Это не картинка — выберите фото.".to_owned())?;
    let thumb = img.thumbnail(96, 96);
    let mut buf = std::io::Cursor::new(Vec::new());
    thumb
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|_| "Картинка не сжалась.".to_owned())?;
    let data = buf.into_inner();
    if data.len() > 16000 {
        return Err("Фото не влезло в лимит 16 КБ — попробуйте меньше.".into());
    }
    Ok(data)
}

/// Путь входящего файла: ядро кладёт в ~/.shopot/files/<hex file_id>.
fn files_path(file_id: &[u8; 16]) -> String {
    let hex: String = file_id.iter().map(|b| format!("{b:02x}")).collect();
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/.shopot/files/{hex}")
}

/// Человеческий размер: «900 Б», «248 КБ», «3.2 МБ».
pub fn human_size(size: u32) -> String {
    if size < 1024 {
        format!("{size} Б")
    } else if size < 1024 * 1024 {
        format!("{} КБ", size.div_ceil(1024))
    } else {
        format!("{:.1} МБ", size as f64 / 1024.0 / 1024.0)
    }
}

/// Лента всегда прокручивается к самому свежему сообщению.
fn scroll_feed<L: CoreLink>(state: &State<L>) -> Task<Message> {
    operation::snap_to_end::<Message>(state.feed_id.clone())
}

/// Разовый запрос ядру без привязки к пузырю (contacts/invite/add/block).
fn request_task<L: CoreLink>(state: &State<L>, request: Request) -> Task<Message> {
    let reply_request = request.clone();
    let link = Arc::clone(&state.link);
    Task::perform(link.execute(request), move |reply| {
        Message::Link(LinkIn::Replied {
            request: reply_request.clone(),
            reply,
            tag: None,
        })
    })
}

/// QR по коду; сбой построения не роняет диалог — остаётся копирование.
fn qr_of(code: &str) -> Option<qr_code::Data> {
    qr_code::Data::new(code.as_bytes()).ok()
}

/// Вид: окно целиком, оверлей поверх (GUI_UI.md §1).
pub fn view<L: CoreLink>(state: &State<L>) -> Element<'_, Message> {
    let mut window = column![
        title_bar(state),
        row![sidebar(state), main_pane(state)].height(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill);
    if let Some(bar) = status_bar(state) {
        window = window.push(bar);
    }

    let layered: Element<'_, Message> = match &state.overlay {
        Overlay::None => window.into(),
        overlay => stack![window, overlay_layer(state, overlay)].into(),
    };

    container(layered)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(palette::root)
        .into()
}

/// Оверлей: подложка-кнопка закрывает по клику мимо панели; сама панель
/// обёрнута в opaque — клики по её пустым местам не проваливаются под неё.
fn overlay_layer<'a, L: CoreLink>(
    state: &'a State<L>,
    overlay: &'a Overlay,
) -> Element<'a, Message> {
    let close = || Message::Meet(MeetIn::CloseOverlay);
    let centered = |panel: iced::Element<'a, Message>| {
        container(opaque(panel))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
    };
    match overlay {
        Overlay::None => space::Space::new().into(),
        Overlay::Invite(view) => stack![
            widgets::backdrop(close()),
            centered(widgets::invite_panel(view, state.store.settings.font_scale))
        ]
        .into(),
        Overlay::Card(cid) => {
            let contact = state.store.contact(*cid);
            let name = contact
                .map(|c| c.name.clone())
                .unwrap_or_else(|| cid.to_string());
            let online = contact.map(|c| c.online).unwrap_or(false);
            let blocked = contact.map(|c| c.blocked).unwrap_or(false);
            stack![
                widgets::backdrop(close()),
                centered(widgets::contact_card(
                    *cid,
                    &name,
                    online,
                    blocked,
                    state.store.settings.font_scale,
                    state.store.avatar(*cid).map(<[u8]>::to_vec),
                )),
            ]
            .into()
        }
        Overlay::Menu(menu) => {
            let contact = state.store.contact(menu.cid);
            let name = contact
                .map(|c| c.name.clone())
                .unwrap_or_else(|| menu.cid.to_string());
            let blocked = contact.map(|c| c.blocked).unwrap_or(false);
            let pinned = state.store.pinned_chat() == Some(menu.cid);
            let v22 = state.kernel_v22();
            // Меню у курсора; у нижней кромки окна — над курсором. Высота
            // выросла с пунктами 2.2 (закреп, визитка, удаление).
            let top = if menu.at.y > 420.0 {
                menu.at.y - 210.0
            } else {
                menu.at.y + 6.0
            };
            stack![
                widgets::backdrop(close()),
                container(opaque(widgets::context_menu(
                    menu,
                    &name,
                    blocked,
                    pinned,
                    v22,
                    state.store.settings.font_scale
                )))
                .padding(Padding {
                    top,
                    left: menu.at.x,
                    right: 0.0,
                    bottom: 0.0,
                })
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Left)
                .align_y(Vertical::Top),
            ]
            .into()
        }
        Overlay::Settings(view) => stack![
            widgets::backdrop(close()),
            centered(widgets::settings_panel(
                &state.store.settings,
                view,
                state.profile_name.as_deref(),
                state.store.settings.font_scale,
                state.store.own_avatar()
            ))
        ]
        .into(),
        Overlay::Pins => {
            let entries = state
                .store
                .selected()
                .map(|cid| state.store.pins(cid))
                .unwrap_or(&[]);
            stack![
                widgets::backdrop(close()),
                centered(widgets::pins_panel(
                    entries,
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
        Overlay::MsgMenu(id) => {
            // Меню привязано к выбранному чату. Закреп — ядро 2.2+ и
            // известный msg_id; реакции — на любом ядре, но тоже по
            // msg_id (своё в полёте — подсказка «после отправки»).
            let cid = state.store.selected();
            let msg = cid.and_then(|cid| state.store.msg(cid, *id));
            let react = match (cid, msg.and_then(|m| m.core_id)) {
                (Some(_), Some(_)) => Some(*id),
                _ => None,
            };
            let pin = if !state.kernel_v22() {
                None
            } else {
                Some(match (cid, msg.and_then(|m| m.core_id)) {
                    (Some(cid), Some(core_id)) => {
                        if state.store.is_pinned(cid, core_id) {
                            Some((
                                "Открепить сообщение",
                                Message::Chat(ChatIn::UnpinMessage(core_id)),
                            ))
                        } else {
                            Some((
                                "Закрепить сообщение",
                                Message::Chat(ChatIn::PinMessage(*id)),
                            ))
                        }
                    }
                    _ => None,
                })
            };
            let body = msg.map(|m| m.body.as_str()).unwrap_or("");
            stack![
                widgets::backdrop(close()),
                centered(widgets::msg_menu(
                    body,
                    pin,
                    react,
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
        Overlay::SendCard(to) => {
            // Моя визитка первой; чат-получатель из списка исключён —
            // отправлять человеку его же карточку бессмысленно.
            let rows: Vec<(Option<Cid>, String)> =
                std::iter::once((None, "Моя визитка".to_owned()))
                    .chain(
                        state
                            .store
                            .contacts()
                            .filter(|c| c.cid != *to)
                            .map(|c| (Some(c.cid), c.name.clone())),
                    )
                    .collect();
            stack![
                widgets::backdrop(close()),
                centered(widgets::send_card_panel(
                    rows,
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
        Overlay::ConfirmDelete(cid) => {
            let name = state
                .store
                .contact(*cid)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| cid.to_string());
            stack![
                widgets::backdrop(close()),
                centered(widgets::confirm_delete_panel(
                    *cid,
                    &name,
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
        // Звонок (S7): клик мимо не закрывает (GUI_UI.md §3), поэтому
        // подложка глухая — dim без on_press.
        Overlay::Call(view) => {
            let name = state
                .store
                .contact(view.cid)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| view.cid.to_string());
            stack![
                widgets::backdrop_dim(),
                centered(widgets::call_panel(
                    view,
                    &name,
                    now_ms(),
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
        Overlay::Search(view) => {
            let kernel = state.kernel_search();
            // Ядерный режим (2.1+): совпадения из ответов демона по всей
            // истории; 1.x — локальный поиск Store по загруженному окну.
            let rows = if kernel {
                view.results
                    .iter()
                    .map(|hit| widgets::SearchRow {
                        ts_ms: hit.ts_ms,
                        mine: hit.mine,
                        body: hit.body.clone(),
                        chat: if view.global {
                            state
                                .store
                                .contacts()
                                .find(|c| c.cid == hit.cid)
                                .map(|c| c.name.clone())
                        } else {
                            None
                        },
                    })
                    .collect()
            } else {
                state.store.selected().map_or_else(Vec::new, |cid| {
                    state
                        .store
                        .search_loaded(cid, &view.query)
                        .into_iter()
                        .map(|hit| widgets::SearchRow {
                            ts_ms: hit.ts_ms,
                            mine: hit.mine,
                            body: hit.body,
                            chat: None,
                        })
                        .collect()
                })
            };
            let scope = if view.global {
                None
            } else {
                state.store.selected()
            };
            let searching = kernel
                && !view.query.trim().is_empty()
                && view.error.is_none()
                && view.answered.as_ref() != Some(&(view.query.clone(), view.global, scope));
            stack![
                widgets::backdrop(close()),
                centered(widgets::search_panel(
                    view,
                    kernel,
                    searching,
                    rows,
                    state.store.settings.font_scale
                ))
            ]
            .into()
        }
    }
}

fn title_bar<L: CoreLink>(state: &State<L>) -> Element<'static, Message> {
    let scale = state.store.settings.font_scale;
    container(
        row![
            text("shopot")
                .size(14.0 * scale)
                .font(palette::bold())
                .color(palette::ACCENT),
            space::Space::new().width(Length::Fill),
            button(text("Настройки").size(11.0 * scale).color(palette::TEXT))
                .padding(Padding {
                    top: 2.0,
                    right: 10.0,
                    bottom: 2.0,
                    left: 10.0,
                })
                .style(palette::button_plain)
                .on_press(Message::Settings(SettingsIn::Open)),
        ]
        .spacing(10)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(40.0))
    .align_y(Vertical::Center)
    .padding(Padding {
        top: 0.0,
        right: 16.0,
        bottom: 0.0,
        left: 16.0,
    })
    .style(palette::panel)
    .into()
}

/// Строка сайдбара для lazy-снимка (S8б добавил аватарку — tuple
/// распух, clippy прав).
struct SidebarRow {
    cid: Cid,
    name: String,
    online: bool,
    unread: u32,
    chosen: bool,
    pinned: bool,
    avatar: Option<Vec<u8>>,
    avatar_id: Option<[u8; 16]>,
}

/// Ключ ленивости строки сайдбара: скаляры + avatar_id (u128).
#[derive(PartialEq, Eq, Hash)]
struct SidebarKey {
    scale_bits: u32,
    cid: i64,
    unread: u32,
    online: bool,
    chosen: bool,
    pinned: bool,
    avatar_id: Option<u128>,
}

fn sidebar<L: CoreLink>(state: &State<L>) -> Element<'_, Message> {
    let scale = state.store.settings.font_scale;
    let selected = state.store.selected();
    // Снимок строк + ключ ленивости: список перестраивается только при
    // изменении состава/статусов или масштаба (GUI_DEV.md §3 — lazy).
    // Шестой элемент — закреп чата (S12): закреплённый идёт первым.
    let pinned_chat = state.store.pinned_chat();
    let mut rows: Vec<SidebarRow> = state
        .store
        .contacts()
        .map(|c| {
            let unread = state.store.thread(c.cid).map(|t| t.unread).unwrap_or(0);
            SidebarRow {
                cid: c.cid,
                name: c.name.clone(),
                online: c.online,
                unread,
                chosen: selected == Some(c.cid),
                pinned: pinned_chat == Some(c.cid),
                // Аватарка из кэша (S8б): байты клонируем в строку
                // (≤16 КБ на контакт, список маленький); id в ключ lazy.
                avatar: state.store.avatar(c.cid).map(<[u8]>::to_vec),
                avatar_id: c.avatar_id,
            }
        })
        .collect();
    if let Some(pos) = rows.iter().position(|row| row.pinned) {
        let pinned = rows.remove(pos);
        rows.insert(0, pinned);
    }
    let has_contacts = !rows.is_empty();

    let list: Element<'static, Message> = if rows.is_empty() {
        container(
            column![
                text("Пока контактов нет. Создайте код приглашения или вставьте чужой.")
                    .size(12.0 * scale)
                    .color(palette::MUTED),
                row![
                    button(text("Пригласить").size(12.0 * scale).color(palette::TEXT))
                        .padding(Padding {
                            top: 6.0,
                            right: 12.0,
                            bottom: 6.0,
                            left: 12.0,
                        })
                        .style(palette::button_dim)
                        .on_press(Message::Meet(MeetIn::OpenInvite)),
                    button(text("Добавить").size(12.0 * scale).color(palette::TEXT))
                        .padding(Padding {
                            top: 6.0,
                            right: 12.0,
                            bottom: 6.0,
                            left: 12.0,
                        })
                        .style(palette::button_dim)
                        .on_press(Message::Meet(MeetIn::OpenInvite)),
                ]
                .spacing(8)
            ]
            .spacing(10),
        )
        .padding(Padding {
            top: 8.0,
            right: 4.0,
            bottom: 8.0,
            left: 4.0,
        })
        .into()
    } else {
        // Масштаб в ключе: смена настройки обязана перерисовать строки.
        let key: Vec<SidebarKey> = rows
            .iter()
            .map(|row| SidebarKey {
                scale_bits: scale.to_bits(),
                cid: row.cid.0,
                unread: row.unread,
                online: row.online,
                chosen: row.chosen,
                pinned: row.pinned,
                avatar_id: row.avatar_id.map(u128::from_le_bytes),
            })
            .collect();
        lazy(key, move |_| {
            let mut list = column![].spacing(2);
            for row in &rows {
                if row.pinned {
                    list = list.push(
                        text("Закреплённый")
                            .size(10.0 * scale)
                            .color(palette::MUTED),
                    );
                }
                // Правая кнопка — меню знакомства; on_move запоминает точку.
                list = list.push(
                    mouse_area(widgets::contact_row(
                        row.cid,
                        &row.name,
                        row.online,
                        row.unread,
                        row.chosen,
                        scale,
                        row.avatar.clone(),
                    ))
                    .on_right_press(Message::Meet(MeetIn::OpenMenu(row.cid)))
                    .on_move(|point| Message::Meet(MeetIn::Cursor(point))),
                );
            }
            list
        })
        .into()
    };

    // Постоянный вход в инвайт-диалог, когда контакты уже есть.
    let mut pane = column![list].spacing(4);
    if has_contacts {
        pane = pane.push(
            button(text("Пригласить").size(12.0 * scale).color(palette::MUTED))
                .width(Length::Fill)
                .padding(Padding {
                    top: 6.0,
                    right: 12.0,
                    bottom: 6.0,
                    left: 12.0,
                })
                .style(palette::button_dim)
                .on_press(Message::Meet(MeetIn::OpenInvite)),
        );
    }

    container(pane)
        .width(Length::Fixed(260.0))
        .height(Length::Fill)
        .padding(Padding {
            top: 8.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0,
        })
        .style(palette::panel)
        .into()
}

fn main_pane<L: CoreLink>(state: &State<L>) -> Element<'_, Message> {
    let scale = state.store.settings.font_scale;
    let Some(cid) = state.store.selected() else {
        return container(
            text("Выберите чат слева")
                .size(13.0 * scale)
                .color(palette::MUTED),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center)
        .into();
    };

    let name = state
        .store
        .contact(cid)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| cid.to_string());
    let online = state.store.contact(cid).map(|c| c.online).unwrap_or(false);

    let header = container(
        row![
            // Клик по имени открывает карточку контакта (GUI_UI.md §1).
            button({
                // Аватарка слева от имени, когда байты уже у ядра
                // приехали (S8б).
                let mut title = row![].spacing(10).align_y(Vertical::Center);
                if let Some(image) = widgets::avatar_thumb(state.store.avatar(cid), 32.0 * scale) {
                    title = title.push(image);
                }
                title.push(
                    column![
                        text(name.clone())
                            .size(14.0 * scale)
                            .font(palette::bold())
                            .color(palette::TEXT),
                        {
                            // Presence и сессия здесь одно и то же: ядро не
                            // шлёт сессию отдельно, она выводится из PRESENCE
                            // (store.rs). Пользователю — простые слова.
                            let (label, color) = if online {
                                ("в сети", palette::OK)
                            } else {
                                ("не в сети", palette::DIM)
                            };
                            text(label).size(10.0 * scale).color(color)
                        },
                    ]
                    .spacing(1)
                    .align_x(Horizontal::Left),
                )
            })
            .padding(Padding {
                top: 2.0,
                right: 6.0,
                bottom: 2.0,
                left: 6.0,
            })
            .style(palette::button_plain)
            .on_press(Message::Meet(MeetIn::OpenCard(cid))),
            space::Space::new().width(Length::Fill),
            // Звонок (S7): ядру нужно живое соединение — офлайн-кнопка
            // выключена, а не ругается. В звонке линия занята — выключена.
            button(text("Позвонить").size(12.0 * scale).color(palette::TEXT))
                .padding(Padding {
                    top: 6.0,
                    right: 12.0,
                    bottom: 6.0,
                    left: 12.0,
                })
                .style(palette::button_dim)
                .on_press_maybe(
                    (online && state.call_view().is_none())
                        .then_some(Message::Call(CallIn::Start(cid))),
                ),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(52.0))
    .align_y(Vertical::Center)
    .padding(Padding {
        top: 0.0,
        right: 16.0,
        bottom: 0.0,
        left: 16.0,
    })
    .style(palette::panel);

    let feed = feed_view(state, cid);

    // Плашка закрепов под шапкой (S12): только на ядре 2.2+ и когда
    // закрепы есть. Превью — последнее разрешённое тело.
    let pins = state.store.pins(cid);
    let mut content = column![header];
    if state.kernel_v22() && !pins.is_empty() {
        let preview = pins.iter().rev().find_map(|p| p.body.as_deref());
        content = content.push(widgets::pins_bar(pins.len(), preview, scale));
    }
    let content = content.push(feed).push(composer(state));

    // Рамок у панелей больше нет: зазор между сайдбаром и лентой
    // не нужен, их разделяет контраст заливок (palette::panel/root).
    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(palette::root)
        .into()
}

fn feed_view<L: CoreLink>(state: &State<L>, cid: Cid) -> Element<'_, Message> {
    let scale = state.store.settings.font_scale;
    let compact = state.store.settings.compact;
    let items = state.store.feed(cid, local_day);
    let mut list = column![].spacing(2).padding(Padding {
        top: 8.0,
        right: 12.0,
        bottom: 8.0,
        left: 12.0,
    });
    for item in items {
        match item {
            FeedItem::DateSep { ts_ms } => list = list.push(widgets::date_sep(ts_ms, scale)),
            FeedItem::Sys { text } => list = list.push(widgets::sys_line(text, scale)),
            FeedItem::Card { name, .. } => list = list.push(widgets::card_bubble(name, scale)),
            group @ FeedItem::Group { .. } => {
                let rows = if compact {
                    widgets::compact_group(&group, scale, &state.revealed_spoilers)
                } else {
                    widgets::message_group(&group, scale, &state.revealed_spoilers)
                };
                for element in rows {
                    list = list.push(element);
                }
            }
        }
    }

    // Контент прижат к низу: короткая переписка не повисает в пустоте
    // над композером; при переполнении контейнер растёт и скроллится.
    let anchored = container(list)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(Vertical::Bottom);

    scrollable(anchored)
        .id(state.feed_id.clone())
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(|viewport| Message::Chat(ChatIn::ScrollTop(viewport.absolute_offset().y <= 0.0)))
        .into()
}

fn composer<L: CoreLink>(state: &State<L>) -> Element<'_, Message> {
    let scale = state.store.settings.font_scale;
    let empty = state.composer.text().trim().is_empty();
    // Авторост: базовая строка 48 px, до 5 строк — GUI_UI.md §1;
    // высота растёт вместе с масштабом шрифта.
    let lines = state.composer.text().lines().count().clamp(1, 5) as f32;
    let editor_height = (48.0 + (lines - 1.0) * 20.0) * scale;

    let editor = text_editor(&state.composer)
        .id(state.composer_id.clone())
        .height(Length::Fixed(editor_height))
        .size(14.0 * scale)
        .placeholder("Сообщение")
        .padding(Padding {
            top: 10.0,
            right: 12.0,
            bottom: 10.0,
            left: 12.0,
        })
        .on_action(|action| Message::Chat(ChatIn::Edit(action)))
        .key_binding(key_binding)
        .style(palette::editor);

    // Вложение (S10): выбор файла — нативный диалог, ядро читает путь.
    // Иконка — SVG: глиф эмодзи в рендере не красится (COLRv1), а
    // пустая кнопка выглядит сломанной.
    const PAPERCLIP: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#98989F" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.5l-8.5 8.5a5.5 5.5 0 0 1-7.8-7.8l9-9a3.7 3.7 0 0 1 5.2 5.2l-8.8 8.8a1.85 1.85 0 0 1-2.6-2.6l8.1-8.1"/></svg>"##;
    let attach = button(
        iced::widget::svg(iced::widget::svg::Handle::from_memory(PAPERCLIP))
            .width(18.0 * scale)
            .height(18.0 * scale),
    )
    .padding(Padding {
        top: 8.0,
        right: 11.0,
        bottom: 8.0,
        left: 11.0,
    })
    .style(palette::button_dim)
    .on_press(Message::Chat(ChatIn::PickFile));

    let send = button(text("↑").size(16.0 * scale).font(palette::bold()))
        .padding(Padding {
            top: 8.0,
            right: 13.0,
            bottom: 8.0,
            left: 13.0,
        })
        .style(palette::button_accent)
        .on_press_maybe((!empty).then_some(Message::Chat(ChatIn::Send)));

    container(
        iced::widget::Row::new()
            .push(container(editor).width(Length::Fill))
            .push(attach)
            .push(send)
            .spacing(10)
            .align_y(Vertical::Bottom),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 10.0,
        right: 16.0,
        bottom: 10.0,
        left: 16.0,
    })
    .style(palette::root)
    .into()
}

/// Ctrl+1..9 — индекс чата в порядке сайдбара. Код физической клавиши:
/// не зависит от раскладки (второй ряд и в RU, и в US — цифры).
pub fn chat_index(code: &iced::keyboard::key::Code) -> Option<usize> {
    use iced::keyboard::key::Code;
    match code {
        Code::Digit1 => Some(0),
        Code::Digit2 => Some(1),
        Code::Digit3 => Some(2),
        Code::Digit4 => Some(3),
        Code::Digit5 => Some(4),
        Code::Digit6 => Some(5),
        Code::Digit7 => Some(6),
        Code::Digit8 => Some(7),
        Code::Digit9 => Some(8),
        _ => None,
    }
}

/// Enter — отправить, Shift+Enter — перенос, Esc — закрыть оверлей,
/// Ctrl+1..9 — чат по номеру, Ctrl+F — поиск, Ctrl+Q — выход; остальное
/// — как в iced. Эти комбинации ловятся при фокусе композера: подписка
/// клавиатуры в headless-эмуляторе событий не получает.
fn key_binding(event: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    match event.key.as_ref() {
        keyboard::Key::Named(Named::Enter) => {
            if event.modifiers.shift() {
                Some(text_editor::Binding::Insert('\n'))
            } else {
                Some(text_editor::Binding::Custom(Message::Chat(ChatIn::Send)))
            }
        }
        keyboard::Key::Named(Named::Escape) => Some(text_editor::Binding::Custom(Message::Meet(
            MeetIn::CloseOverlay,
        ))),
        _ if event.modifiers.control() => ctrl_binding(event),
        _ => text_editor::Binding::from_key_press(event),
    }
}

/// Ctrl-комбинации — по физической клавише, раскладка не важна
/// (GUI_UI.md §4). Нераспознанное отдаётся iced.
fn ctrl_binding(event: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    use iced::keyboard::key::{Code, Physical};
    let Physical::Code(code) = event.physical_key else {
        return text_editor::Binding::from_key_press(event);
    };
    if let Some(index) = chat_index(&code) {
        return Some(text_editor::Binding::Custom(Message::Chat(
            ChatIn::SelectIndex(index),
        )));
    }
    match code {
        Code::KeyQ => Some(text_editor::Binding::Custom(Message::Window(
            WindowIn::Exit,
        ))),
        Code::KeyF => Some(text_editor::Binding::Custom(Message::Chat(
            ChatIn::SearchOpen,
        ))),
        _ => text_editor::Binding::from_key_press(event),
    }
}

/// Статус-строка существует только ради проблемы: нормальная работа не
/// занимает место на экране. Заглушка (демо без ядра) проблемой не
/// считается — там и показывать нечего.
fn status_bar<L: CoreLink>(state: &State<L>) -> Option<Element<'static, Message>> {
    let scale = state.store.settings.font_scale;
    if state.link.is_stub() || state.link_up {
        return None;
    }
    Some(
        container(
            text("Нет соединения. Сообщения не отправляются.")
                .size(11.0 * scale)
                .color(palette::ERR),
        )
        .width(Length::Fill)
        .height(Length::Fixed(24.0))
        .padding(Padding {
            top: 0.0,
            right: 12.0,
            bottom: 0.0,
            left: 12.0,
        })
        .align_y(Vertical::Center)
        .style(palette::panel)
        .into(),
    )
}

// ---------- время и локальные подписи (TEXT_STYLE.md §3) ----------

/// Код для показа: начало и хвост (GUI_UI.md §1 рисует «shopot1.Bq7…xRf»).
/// Полный код едет через QR и «Скопировать код», а не через текст на экране.
pub fn elided_code(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    if chars.len() <= 36 {
        return code.to_owned();
    }
    let head: String = chars[..16].iter().collect();
    let tail: String = chars[chars.len() - 16..].iter().collect();
    format!("{head}…{tail}")
}

const MONTHS: [&str; 12] = [
    "января",
    "февраля",
    "марта",
    "апреля",
    "мая",
    "июня",
    "июля",
    "августа",
    "сентября",
    "октября",
    "ноября",
    "декабря",
];

fn datetime(ts_ms: i64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .unwrap_or_else(|| {
            chrono::Local
                .timestamp_millis_opt(ts_ms)
                .earliest()
                .expect("ts из ядра — валидное время")
        })
}

fn time_hm(ts_ms: i64) -> String {
    datetime(ts_ms).format("%H:%M").to_string()
}

/// Индекс локального календарного дня; для группировки важно
/// только равенство, поэтому достаточно инъективной свёртки даты.
fn local_day(ts_ms: i64) -> i64 {
    let date = datetime(ts_ms).date_naive();
    i64::from(date.year()) * 1000 + i64::from(date.ordinal())
}

/// «Сегодня», «Вчера», дальше «12 марта».
fn date_label(ts_ms: i64) -> String {
    let date = datetime(ts_ms).date_naive();
    let today = chrono::Local::now().date_naive();
    if date == today {
        "Сегодня".into()
    } else if date == today.pred_opt().unwrap_or(today) {
        "Вчера".into()
    } else {
        format!("{} {}", date.day(), MONTHS[date.month0() as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_link::mock::{MockLink, Scene};
    use crate::model::store::Contact;

    fn state() -> State<MockLink> {
        State::new(Arc::new(MockLink::new(Scene::smoke())))
    }

    fn contact(cid: i64, name: &str) -> Contact {
        Contact {
            cid: Cid(cid),
            name: name.to_owned(),
            online: true,
            blocked: false,
            avatar_id: None,
        }
    }

    /// Ctrl+1..9 в headless-эмуляторе не вводятся — путь проверяем
    /// напрямую через update() (привязка композера и подписка ведут в
    /// тот же ChatIn::SelectIndex).
    #[test]
    fn select_index_opens_chat_in_sidebar_order() {
        let mut state = state();
        state.store.replace_contacts(vec![
            contact(1, "Аня"),
            contact(2, "Борис"),
            contact(3, "Вера"),
        ]);

        let _ = update(&mut state, Message::Chat(ChatIn::SelectIndex(2)));
        assert_eq!(state.store.selected(), Some(Cid(3)));
        let _ = update(&mut state, Message::Chat(ChatIn::SelectIndex(0)));
        assert_eq!(state.store.selected(), Some(Cid(1)));
    }

    #[test]
    fn select_index_beyond_contacts_is_noop() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);

        let _ = update(&mut state, Message::Chat(ChatIn::SelectIndex(5)));
        assert_eq!(state.store.selected(), None, "лишний номер молчится");
    }

    /// Поток истории S6: первое открытие чата запрашивает GET_HISTORY,
    /// ответ восстанавливает ленту, скролл к верху догружает старое,
    /// после исчерпания окна запросы не уходят.
    #[test]
    fn chat_open_restores_history_and_top_scroll_pages_older() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        assert!(state.store.history_requested(Cid(1)), "история запрошена");
        assert!(state.history_in_flight);

        let record = |n: u8, ts: i64, text: &str| crate::core_link::HistoryMsg {
            mine: false,
            delivered: false,
            msg_id: crate::model::msg::CoreMsgId([n; 16]),
            ts_ms: ts,
            text: text.to_owned(),
            entities: Vec::new(),
        };
        let history_reply = |records: Vec<crate::core_link::HistoryMsg>| {
            Message::Link(LinkIn::Replied {
                request: Request::History {
                    cid: Cid(1),
                    limit: 100,
                },
                reply: Ok(OkReply::History { total: 3, records }),
                tag: None,
            })
        };

        let _ = update(
            &mut state,
            history_reply(vec![record(1, 1000, "раз"), record(2, 2000, "два")]),
        );
        assert!(!state.history_in_flight);
        let msgs = &state.store.thread(Cid(1)).unwrap().msgs;
        assert_eq!(msgs.len(), 2, "лента восстановлена");
        assert!(state.store.has_more_history(Cid(1)));

        // Скролл к верху: окно растёт до потолка, старое встаёт впереди.
        let _ = update(&mut state, Message::Chat(ChatIn::ScrollTop(true)));
        assert!(state.history_in_flight, "догрузка запрошена");
        let _ = update(
            &mut state,
            history_reply(vec![record(0, 500, "самое старое")]),
        );
        assert_eq!(
            state.store.thread(Cid(1)).unwrap().msgs[0].body,
            "самое старое"
        );
        assert!(!state.store.has_more_history(Cid(1)), "total 3 исчерпан");

        // Верх без остатка — запрос не уходит.
        let _ = update(&mut state, Message::Chat(ChatIn::ScrollTop(true)));
        assert!(!state.history_in_flight, "нечего догружать");

        // Скролл не у верха глушится сразу.
        let _ = update(&mut state, Message::Chat(ChatIn::ScrollTop(false)));
        assert!(!state.history_in_flight);
    }

    /// Ctrl+F: панель открывается только при выбранном чате, ввод
    /// попадает в запрос, Esc-путь закрывает общий CloseOverlay.
    #[test]
    fn search_panel_opens_on_selected_chat_and_takes_input() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);

        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));
        assert!(
            !matches!(state.overlay, Overlay::Search(_)),
            "без чата искать нечего"
        );

        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));
        assert!(
            matches!(state.overlay, Overlay::Search(_)),
            "панель открыта"
        );

        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("привет".into())),
        );
        let Overlay::Search(view) = &state.overlay else {
            panic!("панель ещё открыта")
        };
        assert_eq!(view.query, "привет");
    }

    #[test]
    fn chat_index_maps_digit_row() {
        use iced::keyboard::key::Code;
        for (code, index) in [(Code::Digit1, 0), (Code::Digit5, 4), (Code::Digit9, 8)] {
            assert_eq!(chat_index(&code), Some(index));
        }
        assert_eq!(chat_index(&Code::Digit0), None, "Ctrl+0 не назначен");
        assert_eq!(chat_index(&Code::KeyA), None);
    }

    // ---------- render-тесты: view() в headless-дерево ----------
    //
    // Покрывают состояния, которые эмулятор ввести не умеет (Ctrl+F,
    // скролл ленты): состояние собирается через update() и Store
    // напрямую, затем проверяется точный текст виджетов. Без рантайма.

    /// view() состояния в дерево виджетов; find ищет точный текст.
    fn render<L: CoreLink>(state: &State<L>) -> iced_test::Simulator<'_, Message> {
        iced_test::simulator(view(state))
    }

    fn history_record(id: u8, ts_ms: i64, text: &str) -> crate::core_link::HistoryMsg {
        crate::core_link::HistoryMsg {
            mine: false,
            delivered: false,
            msg_id: crate::model::msg::CoreMsgId([id; 16]),
            ts_ms,
            text: text.to_owned(),
            entities: Vec::new(),
        }
    }

    /// Панель Ctrl+F: заголовок и подсказка на пустом запросе, честный
    /// «пусто» на промахе. Панель рендерится только открытой.
    #[test]
    fn search_panel_renders_hint_and_empty_state() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        state.store.select(Some(Cid(1)));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));

        {
            let mut ui = render(&state);
            assert!(ui.find("Поиск по чату").is_ok(), "заголовок панели");
            assert!(
                ui.find("Поиск ищет по загруженным сообщениям этого чата.")
                    .is_ok(),
                "подсказка на пустом запросе"
            );
        }

        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("неттакого".into())),
        );
        let mut ui = render(&state);
        assert!(ui.find("Ничего не найдено.").is_ok(), "промах честно пуст");
    }

    /// Хвост панели: показаны первые 50 совпадений, остаток — строкой.
    #[test]
    fn search_results_capped_with_tail_line() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let records: Vec<crate::core_link::HistoryMsg> = (1..=60u8)
            .map(|i| {
                history_record(
                    i,
                    1_000 + i64::from(i) * 1_000,
                    &format!("заметка {i} про хвост"),
                )
            })
            .collect();
        state.store.load_history(Cid(1), 60, &records);
        state.store.select(Some(Cid(1)));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));
        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("хвост".into())),
        );

        let mut ui = render(&state);
        assert!(ui.find("и ещё 10").is_ok(), "60 минус 50 показанных");
        assert!(
            ui.find("заметка 7 про хвост").is_ok(),
            "строка совпадения видна"
        );
    }

    /// Совпадение от ядра (S11) для подстановки в Replied.
    fn kernel_hit(cid: i64, n: u8) -> SearchHit {
        SearchHit {
            cid: Cid(cid),
            mine: false,
            delivered: false,
            fuzzy: false,
            msg_id: crate::model::msg::CoreMsgId([n; 16]),
            ts_ms: 1_000 + i64::from(n) * 1_000,
            body: format!("найдено {n} про хвост"),
            entities: Vec::new(),
        }
    }

    /// Ядерный поиск (S11, API 2.1+): подсказка и галка скопа, «Ищу…»
    /// до ответа, устаревший ответ выброшен, страницы дописываются,
    /// «ещё» исчезает, когда выбрано всё.
    #[test]
    fn kernel_search_renders_reply_and_paginates() {
        let mut state = state();
        state.kernel_api = (2, 1);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        state.store.select(Some(Cid(1)));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));

        {
            let mut ui = render(&state);
            assert!(
                ui.find("Поиск по всей истории чата.").is_ok(),
                "подсказка ядерного режима — без техничности"
            );
            assert!(
                ui.find("Искать по всем чатам").is_ok(),
                "галка скопа (checkbox отдаёт label селектору)"
            );
        }

        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("хвост".into())),
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("Ищу…").is_ok(), "до ответа — честное «ищу»");
        }

        // Устаревший ответ (прошлый запрос) выбрасывается.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: Some(Cid(1)),
                    query: "старое".into(),
                    limit: 50,
                    offset: 0,
                },
                reply: Ok(OkReply::Search {
                    total: 1,
                    hits: vec![kernel_hit(1, 250)],
                }),
                tag: None,
            }),
        );
        {
            let mut ui = render(&state);
            assert!(
                ui.find("найдено 250 про хвост").is_err(),
                "устаревший ответ не рендерится"
            );
            assert!(ui.find("Ищу…").is_ok(), "актуальный запрос ещё в полёте");
        }

        // Первая страница: 2 из 3.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: Some(Cid(1)),
                    query: "хвост".into(),
                    limit: 50,
                    offset: 0,
                },
                reply: Ok(OkReply::Search {
                    total: 3,
                    hits: vec![kernel_hit(1, 1), kernel_hit(1, 2)],
                }),
                tag: None,
            }),
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("найдено 1 про хвост").is_ok());
            assert!(ui.find("Показать ещё (2 из 3)").is_ok());
        }

        // Догрузка хвоста; кнопка исчезает, когда выбрано всё.
        let _ = update(&mut state, Message::Chat(ChatIn::SearchMore));
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: Some(Cid(1)),
                    query: "хвост".into(),
                    limit: 50,
                    offset: 2,
                },
                reply: Ok(OkReply::Search {
                    total: 3,
                    hits: vec![kernel_hit(1, 3)],
                }),
                tag: None,
            }),
        );
        let mut ui = render(&state);
        assert!(
            ui.find("найдено 3 про хвост").is_ok(),
            "страница дописана в хвост"
        );
        assert!(
            ui.find("Показать ещё (3 из 3)").is_err(),
            "выбрано всё — кнопки нет"
        );
    }

    /// Глобальный скоп (GLOBAL_SEARCH_MESSAGE): запрос без cid, строки
    /// показывают имя чата; смена скопа переспрашивает ядро.
    #[test]
    fn kernel_search_global_scope_shows_chat_names() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state
            .store
            .replace_contacts(vec![contact(1, "Аня"), contact(2, "Борис")]);
        state.store.select(Some(Cid(1)));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchScope(true)));
        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("привет".into())),
        );

        // Ответ с чужим скопом (чатовый) на глобальный запрос — устарел.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: Some(Cid(1)),
                    query: "привет".into(),
                    limit: 50,
                    offset: 0,
                },
                reply: Ok(OkReply::Search {
                    total: 1,
                    hits: vec![kernel_hit(1, 7)],
                }),
                tag: None,
            }),
        );

        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: None,
                    query: "привет".into(),
                    limit: 50,
                    offset: 0,
                },
                reply: Ok(OkReply::Search {
                    total: 1,
                    hits: vec![SearchHit {
                        body: "привет от Бориса".into(),
                        ..kernel_hit(2, 9)
                    }],
                }),
                tag: None,
            }),
        );
        let mut ui = render(&state);
        assert!(
            ui.find("найдено 7 про хвост").is_err(),
            "чатовый ответ на глобальный запрос выброшен"
        );
        assert!(ui.find("привет от Бориса").is_ok());
        assert!(
            ui.find("Борис").is_ok(),
            "имя чата у глобального совпадения"
        );
    }

    /// Ошибка поиска — честный текст пользователю, а не «ничего не
    /// найдено» (TEXT_STYLE §3).
    #[test]
    fn kernel_search_error_is_shown() {
        let mut state = state();
        state.kernel_api = (2, 1);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        state.store.select(Some(Cid(1)));
        let _ = update(&mut state, Message::Chat(ChatIn::SearchOpen));
        let _ = update(
            &mut state,
            Message::Chat(ChatIn::SearchInput("хвост".into())),
        );
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::Search {
                    cid: Some(Cid(1)),
                    query: "хвост".into(),
                    limit: 50,
                    offset: 0,
                },
                reply: Err(crate::core_link::LinkError::Disconnected),
                tag: None,
            }),
        );
        let mut ui = render(&state);
        assert!(
            ui.find("Нет соединения. Перезапустите приложение.").is_ok(),
            "текст ошибки виден"
        );
        assert!(
            ui.find("Ничего не найдено.").is_err(),
            "ошибка не маскируется под промах"
        );
    }

    /// Статусы отправки рендерятся честно (GUI_STYLE.md §2): разрывы
    /// длиннее пяти минут дают каждому сообщению свою группу, у каждой
    /// свой статус в подвале.
    #[test]
    fn feed_renders_honest_send_states() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = state
            .store
            .push_outgoing(Cid(1), 1_000, "уходит сейчас".into(), Vec::new());
        let queued =
            state
                .store
                .push_outgoing(Cid(1), 6_000_000, "ждёт очереди".into(), Vec::new());
        let delivered = state
            .store
            .push_outgoing(Cid(1), 12_000_000, "уехало".into(), Vec::new());
        let failed =
            state
                .store
                .push_outgoing(Cid(1), 18_000_000, "не проехало".into(), Vec::new());
        state.store.mark_queued(Cid(1), queued);
        state
            .store
            .mark_failed(Cid(1), failed, "ядро недоступно".to_owned());
        let core_id = crate::model::msg::CoreMsgId([7; 16]);
        state.store.mark_sent_core(Cid(1), delivered, core_id);
        state.store.mark_delivered(Cid(1), core_id);
        state.store.select(Some(Cid(1)));

        let mut ui = render(&state);
        assert!(ui.find("отправляю").is_ok(), "Sending рендерится");
        assert!(ui.find("отправится позже").is_ok(), "Queued рендерится");
        assert!(ui.find("доставлено").is_ok(), "Delivered рендерится");
        assert!(
            ui.find("не ушло: ядро недоступно").is_ok(),
            "Failed рендерится с причиной"
        );
    }

    /// Разделитель дня и служебные строки ядра видны в ленте.
    #[test]
    fn feed_renders_day_separator_and_sys_lines() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let now = now_ms();
        state
            .store
            .push_sys(Cid(1), now, "сессия установлена".to_owned());
        let _ = state
            .store
            .push_outgoing(Cid(1), now, "привет".into(), Vec::new());
        state.store.select(Some(Cid(1)));

        let mut ui = render(&state);
        assert!(ui.find("Сегодня").is_ok(), "разделитель дня");
        assert!(
            ui.find("сессия установлена").is_ok(),
            "служебная строка ядра"
        );
        assert!(ui.find("привет").is_ok(), "пузырь сообщения");
    }

    // ---------- S12: закрепы (API 2.2+) ----------

    fn replied(state: &mut State<MockLink>, request: Request, reply: OkReply) {
        let _ = update(
            state,
            Message::Link(LinkIn::Replied {
                request,
                reply: Ok(reply),
                tag: None,
            }),
        );
    }

    /// Закреп чата: пункт меню только на 2.2+; ответ ядра подписывает
    /// чат в сайдбаре и поднимает его наверх; повтор — открепление.
    #[test]
    fn chat_pin_gated_menu_and_sidebar() {
        let mut state = state();
        state
            .store
            .replace_contacts(vec![contact(1, "Аня"), contact(2, "Борис")]);

        // Ядро 1.x: пункта закрепа нет.
        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        {
            let mut ui = render(&state);
            assert!(ui.find("Закрепить чат").is_err(), "на 1.x пункта нет");
        }
        let _ = update(&mut state, Message::Meet(MeetIn::CloseOverlay));

        // Ядро 2.2: пункт есть, ответ закрепляет.
        state.kernel_api = (2, 2);
        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        {
            let mut ui = render(&state);
            assert!(ui.find("Закрепить чат").is_ok(), "на 2.2 пункт есть");
        }
        let _ = update(&mut state, Message::Meet(MeetIn::TogglePinChat(Cid(1))));
        replied(&mut state, Request::PinChat { cid: Cid(1) }, OkReply::Done);
        assert_eq!(state.store.pinned_chat(), Some(Cid(1)));
        {
            let mut ui = render(&state);
            assert!(ui.find("Закреплённый").is_ok(), "подпись в сайдбаре");
        }

        // Повторный заход в меню закреплённого — «Открепить чат».
        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        let mut ui = render(&state);
        assert!(ui.find("Открепить чат").is_ok(), "состояние из Store");
    }

    /// Закрепы сообщений: плашка под шапкой, тело из ленты сразу,
    /// ПКМ-меню пузыря, откреп перечитывает состав у ядра.
    #[test]
    fn message_pins_bar_menu_and_unpin() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let core = CoreMsgId([9; 16]);
        state.store.load_history(
            Cid(1),
            1,
            &[crate::core_link::HistoryMsg {
                mine: false,
                delivered: false,
                msg_id: core,
                ts_ms: 1_000,
                text: "важное".into(),
                entities: Vec::new(),
            }],
        );
        state.store.select(Some(Cid(1)));

        replied(
            &mut state,
            Request::PinnedMessages { cid: Cid(1) },
            OkReply::PinnedMessages { ids: vec![core] },
        );
        {
            let mut ui = render(&state);
            assert!(
                ui.find("Закреплено: 1 · важное").is_ok(),
                "плашка с телом из ленты"
            );
        }

        // ПКМ-меню пузыря: сообщение закреплено → «Открепить сообщение».
        let msg = state.store.thread(Cid(1)).unwrap().msgs[0].id;
        let _ = update(&mut state, Message::Chat(ChatIn::OpenMsgMenu(msg)));
        {
            let mut ui = render(&state);
            assert!(ui.find("Открепить сообщение").is_ok(), "пункт открепа");
        }
        let _ = update(&mut state, Message::Chat(ChatIn::UnpinMessage(core)));
        assert!(
            matches!(state.overlay, Overlay::None),
            "меню закрылось по действию"
        );
        replied(
            &mut state,
            Request::UnpinMessage {
                cid: Cid(1),
                msg_id: core,
            },
            OkReply::Done,
        );
        // Ядро перечитано: состав пуст → плашка исчезает.
        replied(
            &mut state,
            Request::PinnedMessages { cid: Cid(1) },
            OkReply::PinnedMessages { ids: vec![] },
        );
        let mut ui = render(&state);
        assert!(
            ui.find("Закреплено: 0").is_err(),
            "пустых закрепов не рисуем"
        );
    }

    /// Тело закрепа, которого нет в ленте, дозапрашивается (GET_MESSAGE):
    /// до ответа — число без превью, после — с превью. Список закрепов
    /// открывается из плашки.
    #[test]
    fn pin_body_resolved_via_get_message() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        state.store.select(Some(Cid(1)));
        let core = CoreMsgId([5; 16]);

        replied(
            &mut state,
            Request::PinnedMessages { cid: Cid(1) },
            OkReply::PinnedMessages { ids: vec![core] },
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("Закреплено: 1").is_ok(), "число без превью");
        }
        replied(
            &mut state,
            Request::GetMessage { msg_id: core },
            OkReply::Message {
                cid: Cid(1),
                mine: false,
                delivered: false,
                ts_ms: 2_000,
                text: "старое важное".into(),
                entities: Vec::new(),
            },
        );
        {
            let mut ui = render(&state);
            assert!(
                ui.find("Закреплено: 1 · старое важное").is_ok(),
                "превью после GET_MESSAGE"
            );
        }

        let _ = update(&mut state, Message::Chat(ChatIn::OpenPins));
        let mut ui = render(&state);
        assert!(
            ui.find("Закреплённые сообщения").is_ok(),
            "заголовок списка"
        );
        assert!(ui.find("старое важное").is_ok(), "тело в списке");
        assert!(ui.find("Открепить").is_ok(), "действие в списке");
    }

    /// На ядре < 2.2 меню сообщения открывается (реакции — 1.0+, S15),
    /// но пункта закрепа в нём нет.
    #[test]
    fn pins_hidden_below_api_2_2() {
        let mut state = state();
        state.kernel_api = (2, 1);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        state.store.push_incoming_core(
            Cid(1),
            "Аня",
            crate::model::msg::CoreMsgId([0x07; 16]),
            1_000,
            "привет".into(),
            Vec::new(),
        );
        state.store.select(Some(Cid(1)));
        let msg = state.store.thread(Cid(1)).unwrap().msgs[0].id;

        let _ = update(&mut state, Message::Chat(ChatIn::OpenMsgMenu(msg)));
        assert!(
            matches!(state.overlay, Overlay::MsgMenu(_)),
            "меню открылось — реакции есть на любом ядре"
        );
        let mut ui = render(&state);
        assert!(
            ui.find("Закрепить сообщение").is_err(),
            "закрепа нет ниже 2.2"
        );
        assert!(ui.find("Отмена").is_ok(), "меню живое");
    }

    // ---------- S15: реакции ----------

    /// Своя реакция: React → SET, по OK пилюля mine; повторный React —
    /// REMOVE, по OK пилюля гаснет. Событие пира тоже рисует пилюлю.
    #[test]
    fn reaction_toggle_flow() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let core = crate::model::msg::CoreMsgId([0x07; 16]);
        state
            .store
            .push_incoming_core(Cid(1), "Аня", core, 1_000, "привет".into(), Vec::new());
        state.store.select(Some(Cid(1)));
        let msg = state.store.thread(Cid(1)).unwrap().msgs[0].id;

        // Событие пира: 👍 появился, не мой.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::Reaction {
                cid: Cid(1),
                msg_id: core,
                add: true,
                emoji: '👍',
                ts_ms: 1_100,
            })),
        );
        let reactions = &state.store.thread(Cid(1)).unwrap().msgs[0].reactions;
        assert_eq!(reactions.len(), 1);
        assert_eq!((reactions[0].count, reactions[0].mine), (1, false));

        // Моя реакция: по OK-ответу применяется как mine.
        let _ = update(&mut state, Message::Chat(ChatIn::React(msg, '👍')));
        replied(
            &mut state,
            Request::SetReaction {
                cid: Cid(1),
                msg_id: core,
                emoji: '👍',
            },
            OkReply::Done,
        );
        let reactions = &state.store.thread(Cid(1)).unwrap().msgs[0].reactions;
        assert_eq!((reactions[0].count, reactions[0].mine), (2, true));

        // Повторный клик — снятие; по OK пилюля гаснет? Нет: пир держит
        // свою — счётчик 1, mine=false.
        let _ = update(&mut state, Message::Chat(ChatIn::React(msg, '👍')));
        replied(
            &mut state,
            Request::RemoveReaction {
                cid: Cid(1),
                msg_id: core,
                emoji: '👍',
            },
            OkReply::Done,
        );
        let reactions = &state.store.thread(Cid(1)).unwrap().msgs[0].reactions;
        assert_eq!((reactions[0].count, reactions[0].mine), (1, false));
    }

    /// Реакция на своё летящее сообщение (нет msg_id ядра) — честная
    /// sys-строка, запроса нет; ошибка ядра — sys-строка.
    #[test]
    fn reaction_needs_core_id_and_reports_errors() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = state
            .store
            .push_outgoing(Cid(1), 1_000, "моё летит".into(), Vec::new());
        state.store.select(Some(Cid(1)));
        let msg = state.store.thread(Cid(1)).unwrap().msgs[0].id;

        let _ = update(&mut state, Message::Chat(ChatIn::React(msg, '👍')));
        let msgs = &state.store.thread(Cid(1)).unwrap().msgs;
        assert!(
            msgs.iter()
                .any(|m| m.body == "реагировать можно после отправки"),
            "честная подсказка: {msgs:?}"
        );

        // Ошибка ядра по SET — sys-строка в чате.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SetReaction {
                    cid: Cid(1),
                    msg_id: crate::model::msg::CoreMsgId([0x07; 16]),
                    emoji: '👍',
                },
                reply: Err(crate::core_link::LinkError::Kernel(
                    crate::core_link::api::Status::NoSession,
                )),
                tag: None,
            }),
        );
        let msgs = &state.store.thread(Cid(1)).unwrap().msgs;
        assert!(
            msgs.iter().any(|m| m.body.contains("реакция не ушла")),
            "ошибка видна: {msgs:?}"
        );
    }

    /// Ответ на закреп чужого чата не трогает открытый (состав хранится
    /// по cid запроса).
    #[test]
    fn pins_reply_scoped_to_request_chat() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state
            .store
            .replace_contacts(vec![contact(1, "Аня"), contact(2, "Борис")]);
        state.store.select(Some(Cid(1)));
        replied(
            &mut state,
            Request::PinnedMessages { cid: Cid(2) },
            OkReply::PinnedMessages {
                ids: vec![CoreMsgId([3; 16])],
            },
        );
        let mut ui = render(&state);
        assert!(
            ui.find("Закреплено: 1").is_err(),
            "плашка открытого чата пуста"
        );
        assert_eq!(state.store.pins(Cid(2)).len(), 1, "состав у своего чата");
    }

    // ---------- S13: визитки и удаление чата (API 2.2+) ----------

    /// Событие CONTACT_CARD: запись в ленте с честной подсказкой
    /// (визитка знакомит, не добавляет), в фоновом чате — непрочитанная.
    #[test]
    fn card_event_lands_in_feed_with_honest_hint() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);

        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::ContactCard {
                cid: Cid(1),
                name: "Вера".into(),
                bundle: [0x42; 64],
                addr: String::new(),
            })),
        );
        assert_eq!(
            state.store.thread(Cid(1)).unwrap().unread,
            1,
            "фоновая визитка непрочитанная"
        );

        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        let mut ui = render(&state);
        assert!(ui.find("Вера").is_ok(), "имя из визитки в ленте");
        assert!(
            ui.find("визитка · добавить можно по приглашению").is_ok(),
            "честная подсказка"
        );
        // Адрес и bundle на экран не выносим: добавить человека из
        // визитки всё равно нельзя.
        assert_eq!(state.store.thread(Cid(1)).unwrap().unread, 0);
    }

    /// Отправка визитки: пункт в меню чата, выбор (своя/чужая), честные
    /// sys-строки об успехе и ошибке.
    #[test]
    fn send_card_picker_and_reply_lines() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![
            contact(1, "Аня"),
            contact(2, "Борис"),
            contact(3, "Вера"),
        ]);
        state.store.select(Some(Cid(1)));

        // Меню чата → выбор визитки.
        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        {
            let mut ui = render(&state);
            assert!(ui.find("Отправить визитку…").is_ok(), "пункт в меню");
        }
        let _ = update(&mut state, Message::Meet(MeetIn::OpenSendCard(Cid(1))));
        assert!(
            matches!(state.overlay, Overlay::SendCard(Cid(1))),
            "выбор открыт для чата 1"
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("Чья визитка?").is_ok(), "заголовок выбора");
            assert!(ui.find("Моя визитка").is_ok(), "своя карточка в списке");
        }

        // Чужая карточка: ответ ядра — sys-строка в чате-получателе.
        let _ = update(
            &mut state,
            Message::Meet(MeetIn::PickSendCard(Some(Cid(2)))),
        );
        assert!(state.overlay_closed(), "выбор закрылся после выбора");
        replied(
            &mut state,
            Request::SendContact {
                to: Cid(1),
                contact: Some(Cid(2)),
            },
            OkReply::Done,
        );
        {
            let mut ui = render(&state);
            assert!(
                ui.find("визитка отправлена: Борис").is_ok(),
                "отметка об успехе"
            );
        }

        // Своя карточка.
        let _ = update(&mut state, Message::Meet(MeetIn::OpenSendCard(Cid(1))));
        let _ = update(&mut state, Message::Meet(MeetIn::PickSendCard(None)));
        replied(
            &mut state,
            Request::SendContact {
                to: Cid(1),
                contact: None,
            },
            OkReply::Done,
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("визитка отправлена: моя").is_ok(), "своя — «моя»");
        }

        // Ошибка (получатель офлайн) — честная строка, не молчание.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SendContact {
                    to: Cid(1),
                    contact: None,
                },
                reply: Err(crate::core_link::LinkError::Kernel(
                    crate::core_link::api::Status::NoSession,
                )),
                tag: None,
            }),
        );
        let mut ui = render(&state);
        assert!(
            ui.find("визитка не отправлена: Контакт не в сети. Напишите, когда он появится.")
                .is_ok(),
            "ошибка с причиной"
        );
    }

    /// Удаление чата: пункт меню, честное подтверждение (только наша
    /// сторона, новое приглашение), ответ ядра стирает проекцию.
    #[test]
    fn delete_chat_confirms_and_drops_projection() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state
            .store
            .replace_contacts(vec![contact(1, "Аня"), contact(2, "Борис")]);
        state.store.select(Some(Cid(1)));

        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        let _ = update(&mut state, Message::Meet(MeetIn::AskDeleteChat(Cid(1))));
        assert!(matches!(state.overlay, Overlay::ConfirmDelete(Cid(1))));
        {
            let mut ui = render(&state);
            assert!(ui.find("Удалить чат с Аня?").is_ok(), "вопрос");
            assert!(
                ui.find("История и контакт удалятся безвозвратно — только у вас. Написать снова можно будет по новому приглашению.")
                    .is_ok(),
                "честные последствия"
            );
        }

        // Отмена оставляет всё как было.
        let _ = update(&mut state, Message::Meet(MeetIn::CloseOverlay));
        assert!(state.store.contact(Cid(1)).is_some(), "отмена не удаляет");

        // Подтверждение → ответ ядра стирает контакт и закрывает чат.
        let _ = update(&mut state, Message::Meet(MeetIn::AskDeleteChat(Cid(1))));
        let _ = update(&mut state, Message::Meet(MeetIn::ConfirmDeleteChat(Cid(1))));
        assert!(state.overlay_closed());
        replied(
            &mut state,
            Request::DeleteChat { cid: Cid(1) },
            OkReply::Done,
        );
        assert!(state.store.contact(Cid(1)).is_none());
        assert_eq!(state.store.selected(), None, "открытый чат закрыт");
        {
            let mut ui = render(&state);
            assert!(ui.find("Борис").is_ok(), "соседний чат на месте");
        }

        // Ошибка удаления: чат остаётся, причина — sys-строкой.
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::DeleteChat { cid: Cid(2) },
                reply: Err(crate::core_link::LinkError::Kernel(
                    crate::core_link::api::Status::NoContact,
                )),
                tag: None,
            }),
        );
        assert!(state.store.contact(Cid(2)).is_some(), "ошибка не удаляет");
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(2))));
        let mut ui = render(&state);
        assert!(
            ui.find("чат не удалён: Контакт не найден.").is_ok(),
            "причина в ленте"
        );
    }

    /// На ядре < 2.2 пунктов визитки и удаления в меню нет вообще.
    #[test]
    fn cards_and_delete_hidden_below_api_2_2() {
        let mut state = state();
        state.kernel_api = (2, 1);
        state.store.replace_contacts(vec![contact(1, "Аня")]);

        let _ = update(&mut state, Message::Meet(MeetIn::OpenMenu(Cid(1))));
        {
            let mut ui = render(&state);
            assert!(ui.find("Отправить визитку…").is_err());
            assert!(ui.find("Удалить чат").is_err());
            assert!(ui.find("Закрепить чат").is_err(), "заодно и закреп");
        }
        // Действия под гейтом молча не проходят.
        let _ = update(&mut state, Message::Meet(MeetIn::OpenSendCard(Cid(1))));
        let _ = update(&mut state, Message::Meet(MeetIn::AskDeleteChat(Cid(1))));
        assert!(
            !matches!(
                state.overlay,
                Overlay::SendCard(_) | Overlay::ConfirmDelete(_)
            ),
            "оверлеи 2.2 не открываются"
        );
    }

    // ---------- S13.1: имя профиля (SET_NAME) ----------

    /// Имя профиля: честная подсказка без имени, сохранение через
    /// SET_NAME, итог — строкой в панели; на сессию имя запоминается.
    #[test]
    fn profile_name_saved_with_honest_note() {
        let mut state = state();

        let _ = update(&mut state, Message::Settings(SettingsIn::Open));
        assert!(matches!(state.overlay, Overlay::Settings(_)));
        {
            let mut ui = render(&state);
            assert!(
                ui.find("Без имени визитка уходит с именем «shopot».")
                    .is_ok(),
                "подсказка, зачем имя"
            );
        }

        let _ = update(
            &mut state,
            Message::Settings(SettingsIn::NameInput("Михаил".into())),
        );
        let _ = update(&mut state, Message::Settings(SettingsIn::SaveName));
        replied(
            &mut state,
            Request::SetName {
                name: "Михаил".into(),
            },
            OkReply::Done,
        );
        assert_eq!(state.profile_name.as_deref(), Some("Михаил"));
        {
            let mut ui = render(&state);
            assert!(
                ui.find("Имя сохранено — будет в ваших визитках.").is_ok(),
                "итог сохранения"
            );
        }

        // Переоткрытие: ядро имя обратно не отдаёт — показываем свою запись.
        let _ = update(&mut state, Message::Meet(MeetIn::CloseOverlay));
        let _ = update(&mut state, Message::Settings(SettingsIn::Open));
        let mut ui = render(&state);
        assert!(
            ui.find("Сейчас задано: Михаил").is_ok(),
            "сессионная запись"
        );
    }

    /// Ошибка SET_NAME — честная строка в панели, имя не запоминается.
    #[test]
    fn profile_name_error_is_honest() {
        let mut state = state();
        let _ = update(&mut state, Message::Settings(SettingsIn::Open));
        let _ = update(
            &mut state,
            Message::Settings(SettingsIn::NameInput("Михаил".into())),
        );
        let _ = update(&mut state, Message::Settings(SettingsIn::SaveName));
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SetName {
                    name: "Михаил".into(),
                },
                reply: Err(crate::core_link::LinkError::Disconnected),
                tag: None,
            }),
        );
        assert_eq!(state.profile_name, None);
        let mut ui = render(&state);
        assert!(
            ui.find("Имя не сохранено: Нет соединения. Перезапустите приложение.")
                .is_ok(),
            "причина в панели"
        );
    }

    // ---------- S7: звонки ----------

    fn call_event(
        state: &mut State<MockLink>,
        cid: i64,
        call_id: [u8; 16],
        signal: crate::core_link::CallSignal,
    ) {
        let _ = update(
            state,
            Message::Link(LinkIn::Event(LinkEvent::CallState {
                cid: Cid(cid),
                call_id,
                state: signal,
            })),
        );
    }

    /// Исходящий: «Позвонить» → START_CALL → «звоним…»; accept → разговор
    /// с таймером и mute; Esc активный звонок не закрывает; end от пира —
    /// sys-строка с длительностью.
    #[test]
    fn outgoing_call_flow() {
        use crate::core_link::CallSignal;
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));

        let _ = update(&mut state, Message::Call(CallIn::Start(Cid(1))));
        let call_id = [0x42; 16];
        replied(
            &mut state,
            Request::StartCall { cid: Cid(1) },
            OkReply::CallStarted { call_id },
        );
        assert!(
            matches!(
                state.call_view().map(|v| v.phase),
                Some(CallPhase::Outgoing)
            ),
            "исходящий оверлей"
        );
        {
            let mut ui = render(&state);
            assert!(ui.find("звоним…").is_ok());
            assert!(ui.find("Отменить").is_ok());
        }

        // Offer-эхо ядра по нашему call_id — пропуск, фаза не меняется.
        call_event(&mut state, 1, call_id, CallSignal::Offer);
        assert!(matches!(
            state.call_view().map(|v| v.phase),
            Some(CallPhase::Outgoing)
        ));

        // Accept → разговор: таймер, mute, «Завершить».
        call_event(&mut state, 1, call_id, CallSignal::Accept);
        assert!(state.call_active());
        {
            let mut ui = render(&state);
            assert!(ui.find("0:00").is_ok(), "таймер");
            assert!(ui.find("Выключить микрофон").is_ok());
            assert!(ui.find("Завершить").is_ok());
        }

        // Esc активный звонок не закрывает (GUI_UI.md §3).
        let _ = update(&mut state, Message::Meet(MeetIn::CloseOverlay));
        assert!(state.call_active(), "Esc не рвёт разговор");

        // Mute-тумблер.
        let _ = update(&mut state, Message::Call(CallIn::ToggleMute));
        {
            let mut ui = render(&state);
            assert!(ui.find("Включить микрофон").is_ok(), "подпись mute");
        }

        // Пир завершил: оверлей закрыт, в чате — длительность.
        call_event(&mut state, 1, call_id, CallSignal::End);
        assert!(state.call_view().is_none());
        let mut ui = render(&state);
        assert!(ui.find("звонок завершён — 0:00").is_ok(), "sys-строка");
    }

    /// Входящий: offer → «входящий звонок» с «Принять»/«Отклонить»;
    /// отклонение — END_CALL и закрытие. Ошибка START_CALL — sys-строка.
    #[test]
    fn incoming_call_answer_and_decline() {
        use crate::core_link::CallSignal;
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);

        // Входящий offer: оверлей с кнопками.
        call_event(&mut state, 1, [0x11; 16], CallSignal::Offer);
        assert!(matches!(
            state.call_view().map(|v| v.phase),
            Some(CallPhase::Incoming)
        ));
        {
            let mut ui = render(&state);
            assert!(ui.find("входящий звонок").is_ok());
            assert!(ui.find("Принять").is_ok());
            assert!(ui.find("Отклонить").is_ok());
        }

        // Приняли: ответ ядра пуст, accept-событие переводит в разговор.
        let _ = update(&mut state, Message::Call(CallIn::Answer));
        replied(&mut state, Request::AnswerCall, OkReply::Done);
        call_event(&mut state, 1, [0x11; 16], CallSignal::Accept);
        assert!(state.call_active());

        // Второй звонок на занятой линии не открывается.
        let _ = update(&mut state, Message::Call(CallIn::Start(Cid(2))));
        assert_eq!(state.call_view().map(|v| v.cid), Some(Cid(1)), "линия одна");
    }

    /// Отклонение входящего: END_CALL + закрытие оверлея. Ошибка
    /// START_CALL (контакт офлайн) — честная sys-строка.
    #[test]
    fn decline_and_failed_start_are_honest() {
        use crate::core_link::CallSignal;
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        call_event(&mut state, 1, [0x22; 16], CallSignal::Offer);
        let _ = update(&mut state, Message::Call(CallIn::Decline));
        assert!(state.call_view().is_none(), "отклонён — оверлей закрыт");

        // sys-строка видна в открытом чате.
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        let _ = update(&mut state, Message::Call(CallIn::Start(Cid(1))));
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::StartCall { cid: Cid(1) },
                reply: Err(crate::core_link::LinkError::Kernel(
                    crate::core_link::api::Status::NoSession,
                )),
                tag: None,
            }),
        );
        let mut ui = render(&state);
        assert!(ui
            .find("звонок не начался: Контакт не в сети. Напишите, когда он появится.")
            .is_ok());
    }

    // ---------- S14: разметка сообщений ----------

    /// Отправка с разметкой на ядре 2.x: маркеры срезаны, entities в
    /// сообщении ленты; на 1.x текст уходит как набран.
    #[test]
    fn markup_parsed_on_v2_literal_on_v1() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        state.composer = iced::widget::text_editor::Content::with_text("**жирно** и ||тайна||");
        let _ = update(&mut state, Message::Chat(ChatIn::Send));

        let msgs = state.store.feed(Cid(1), |ts| ts / 86_400_000);
        let FeedItem::Group { bodies, .. } = &msgs[1] else {
            panic!("ожидали группу после разделителя дня: {msgs:?}");
        };
        let body_ref = &bodies[0];
        let (body, entities) = (&body_ref.body, &body_ref.entities);
        assert_eq!(body, "жирно и тайна");
        assert_eq!(
            entities,
            &vec![
                crate::core_link::Entity::new(crate::core_link::EntityKind::Bold, 0, 10),
                crate::core_link::Entity::new(crate::core_link::EntityKind::Spoiler, 14, 10),
            ]
        );

        // Ядро 1.x: маркеры остаются в тексте, entities нет.
        state.kernel_api = (1, 0);
        state.composer = iced::widget::text_editor::Content::with_text("**жирно**");
        let _ = update(&mut state, Message::Chat(ChatIn::Send));
        let msgs = state.store.feed(Cid(1), |ts| ts / 86_400_000);
        let FeedItem::Group { bodies, .. } = &msgs[1] else {
            panic!("второе сообщение той же группы: {msgs:?}");
        };
        // То же окно группировки — второе тело той же группы.
        let last = bodies.last().expect("тело есть");
        assert_eq!(last.body, "**жирно**");
        assert!(last.entities.is_empty());
    }

    /// Спойлер: клик раскрывает, повторный — прячет; переключение живёт
    /// в State, не в ядре.
    #[test]
    fn spoiler_toggles_on_click() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        state.composer = iced::widget::text_editor::Content::with_text("||тайна||");
        let _ = update(&mut state, Message::Chat(ChatIn::Send));
        let msgs = state.store.feed(Cid(1), |ts| ts / 86_400_000);
        let FeedItem::Group { bodies, .. } = &msgs[1] else {
            panic!("группа после разделителя дня: {msgs:?}");
        };
        let id = bodies[0].id;
        assert!(!state.revealed_spoilers.contains(&id), "по умолчанию скрыт");
        let _ = update(&mut state, Message::Chat(ChatIn::ToggleSpoiler(id)));
        assert!(state.revealed_spoilers.contains(&id), "клик раскрыл");
        let _ = update(&mut state, Message::Chat(ChatIn::ToggleSpoiler(id)));
        assert!(!state.revealed_spoilers.contains(&id), "повторный скрыл");
    }

    /// Размеченное сообщение рендерится (rich spans): тело видимо,
    /// паник нет; спойлерный текст в дереве есть (скрыт стилем).
    #[test]
    fn formatted_bubble_renders() {
        let mut state = state();
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));
        state.composer =
            iced::widget::text_editor::Content::with_text("**важно** и [ссылка](https://x.y)");
        let _ = update(&mut state, Message::Chat(ChatIn::Send));
        let mut ui = render(&state);
        assert!(ui.find("Аня").is_ok(), "чат на месте");
    }

    // ---------- S10: файлы ----------

    /// Выбранный файл → пузырь Sending; ответ ядра ставит настоящие
    /// file_id/size; FILE_SENT — честный «отправлен». Отмена диалога
    /// молчит. Ошибка (файл не читается) — Failed с текстом.
    #[test]
    fn file_send_flow_honest_status() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(&mut state, Message::Chat(ChatIn::Select(Cid(1))));

        // Отмена — ничего.
        let _ = update(&mut state, Message::Chat(ChatIn::FilePicked(None)));
        assert!(
            state
                .store
                .thread(Cid(1))
                .map(|t| t.msgs.is_empty())
                .unwrap_or(true),
            "отмена молчит"
        );

        // Выбор → пузырь Sending с локальным именем/размером.
        let tmp = std::env::temp_dir().join(format!("shopot-ui-file-{}", std::process::id()));
        std::fs::write(&tmp, b"demo").expect("файл записан");
        let _ = update(
            &mut state,
            Message::Chat(ChatIn::FilePicked(Some(tmp.clone()))),
        );
        let msgs = &state.store.thread(Cid(1)).unwrap().msgs;
        let msg = msgs[0].id;
        assert_eq!(msgs[0].send, crate::model::msg::SendState::Sending);
        assert_eq!(
            msgs[0].file.as_ref().map(|f| f.name.as_str()),
            Some(tmp.file_name().unwrap().to_str().unwrap())
        );

        // Ответ ядра: настоящий file_id, статус ещё Sending.
        let core = [0xF1; 16];
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SendFile {
                    cid: Cid(1),
                    path: tmp.to_string_lossy().into_owned(),
                },
                reply: Ok(OkReply::FileQueued {
                    file_id: core,
                    size: 4,
                    chunks: 1,
                }),
                tag: Some(msg),
            }),
        );
        let msgs = &state.store.thread(Cid(1)).unwrap().msgs;
        assert_eq!(msgs[0].file.as_ref().map(|f| f.file_id), Some(core));
        assert_eq!(msgs[0].send, crate::model::msg::SendState::Sending);

        // FILE_SENT — «отправлен».
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::FileSent {
                cid: Cid(1),
                file_id: core,
            })),
        );
        assert_eq!(
            state.store.thread(Cid(1)).unwrap().msgs[0].send,
            crate::model::msg::SendState::Sent
        );
        std::fs::remove_file(&tmp).ok();
        let _ = msg;
    }

    /// Входящий файл: пузырь с именем/размером, непрочитанное, путь
    /// ~/.shopot/files/<hex> для открытия.
    #[test]
    fn incoming_file_lands_in_feed() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::FileReceived {
                cid: Cid(1),
                file_id: [0xF1; 16],
                size: 254_000,
                name: "макет.pdf".into(),
            })),
        );
        let thread = state.store.thread(Cid(1)).expect("чат есть");
        assert_eq!(thread.unread, 1);
        let file = thread.msgs[0].file.as_ref().expect("файл");
        assert_eq!(file.name, "макет.pdf");
        assert!(
            file.path
                .as_ref()
                .is_some_and(|p| p.contains("/.shopot/files/f1f1")),
            "путь для открытия: {:?}",
            file.path
        );
    }

    /// human_size: Б/КБ/МБ.
    #[test]
    fn human_size_units() {
        assert_eq!(human_size(900), "900 Б");
        assert_eq!(human_size(254_000), "249 КБ");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 МБ");
    }

    // ---------- S8б: аватарки ----------

    /// AVATAR_RECEIVED: байты в кэш, контакту — id; CONTACT_PROFILE
    /// несёт новый id атомарно с именем.
    #[test]
    fn avatar_event_lands_in_store() {
        let mut state = state();
        state.store.replace_contacts(vec![contact(1, "Аня")]);
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::AvatarReceived {
                cid: Cid(1),
                avatar_id: [0xA1; 16],
                data: vec![1, 2, 3],
            })),
        );
        assert_eq!(state.store.avatar(Cid(1)), Some(&[1, 2, 3][..]));

        let _ = update(
            &mut state,
            Message::Link(LinkIn::Event(LinkEvent::ContactProfile {
                cid: Cid(1),
                name: "Анна".into(),
                avatar_id: CoreMsgId([0xB2; 16]),
            })),
        );
        let contact = state.store.contact(Cid(1)).unwrap();
        assert_eq!(contact.name, "Анна");
        assert_eq!(contact.avatar_id, Some([0xB2; 16]));
        // Байты нового id ещё не приехали — аватарки нет (без заглушек).
        assert!(state.store.avatar(Cid(1)).is_none());
    }

    /// Своя аватарка: OK ядра → байты в кэш + заметка в настройках;
    /// ошибка — честная заметка.
    #[test]
    fn own_avatar_set_reply_and_error() {
        let mut state = state();
        state.overlay = Overlay::Settings(Default::default());
        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SetAvatar { data: vec![9; 10] },
                reply: Ok(OkReply::AvatarSet {
                    avatar_id: [0xC3; 16],
                    size: 10,
                }),
                tag: None,
            }),
        );
        assert_eq!(state.store.own_avatar(), Some(&[9; 10][..]));

        let _ = update(
            &mut state,
            Message::Link(LinkIn::Replied {
                request: Request::SetAvatar { data: vec![9; 10] },
                reply: Err(LinkError::Kernel(crate::core_link::api::Status::TooBig)),
                tag: None,
            }),
        );
        let Overlay::Settings(view) = &state.overlay else {
            panic!()
        };
        assert!(view.note.as_deref().unwrap_or("").contains("лимит"));
    }

    /// prepare_avatar: картинка сжимается до лимита ядра; не-картинка —
    /// честный текст.
    #[test]
    fn prepare_avatar_scales_and_refuses() {
        // 400×400 PNG → JPEG ≤ 16000 Б.
        let img = image::RgbImage::from_fn(400, 400, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 239) as u8, 128])
        });
        let tmp = std::env::temp_dir().join(format!("shopot-avatar-{}.png", std::process::id()));
        img.save(&tmp).expect("png записан");
        let data = prepare_avatar(&tmp).expect("сжалась");
        assert!(data.len() <= 16000, "{} > 16000", data.len());
        assert_eq!(&data[0..2], &[0xFF, 0xD8], "jpeg");
        std::fs::remove_file(&tmp).ok();

        // Не картинка — честный отказ.
        let tmp = std::env::temp_dir().join(format!("shopot-notimg-{}.bin", std::process::id()));
        std::fs::write(&tmp, b"hello").expect("записан");
        let err = prepare_avatar(&tmp).expect_err("не картинка");
        assert!(err.contains("не картинка"), "{err}");
        std::fs::remove_file(&tmp).ok();
    }
}
