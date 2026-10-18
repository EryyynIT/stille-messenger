//! Виджет-компоненты: не знают про протокол, читают только Store
//! и переданные данные (GUI_UI.md §2).

use iced::alignment::Horizontal;
use iced::widget::{
    button, checkbox, column, container, mouse_area, qr_code, row, scrollable, space, text,
    text_input,
};
use iced::{Element, Length, Padding};

use crate::model::cid::Cid;
use crate::model::feed::FeedItem;
use crate::model::msg::SendState;
use crate::model::settings::Settings;
use crate::model::store::PinEntry;
use crate::ui::palette;
use std::collections::{BTreeSet, HashSet};

use iced::font::{Style as FontStyle, Weight};
use iced::widget::rich_text;
use iced::widget::text::Span;
use iced::Font;

use crate::core_link::{Entity, EntityKind};
use crate::model::msg::{CoreMsgId, MsgId, Reaction};
use crate::ui::{
    time_hm, CallIn, CallPhase, CallView, ChatIn, InviteView, MeetIn, MenuView, Message,
    SearchView, SettingsIn, SettingsView,
};

/// Строка сайдбара: точка presence, имя, бейдж непрочитанных. Выбор —
/// капсульная заливка всей строки (стиль chat_row), без полос с краю.
/// Высота карточки фиксированная (правка осмотра окна: строки
/// растягивались на всю высоту сайдбара).
/// Аватарка из байтов кэша (S8б): квадрат size×size; без байтов — None
/// (рисуем как раньше, без заглушек).
pub fn avatar_thumb(bytes: Option<&[u8]>, size: f32) -> Option<Element<'static, Message>> {
    let bytes = bytes?;
    Some(
        iced::widget::image(iced::widget::image::Handle::from_bytes(bytes.to_vec()))
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .into(),
    )
}

/// Сокращает только экранное представление имени. Полное имя остаётся в Store
/// и доступно карточке контакта.
pub fn display_name(name: &str, max_chars: usize) -> String {
    let mut chars = name.chars();
    let visible: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        let keep = max_chars.saturating_sub(1);
        let prefix: String = name.chars().take(keep).collect();
        format!("{prefix}…")
    } else {
        visible
    }
}

pub fn contact_row(
    cid: Cid,
    name: &str,
    online: bool,
    unread: u32,
    selected: bool,
    scale: f32,
    avatar: Option<Vec<u8>>,
) -> Element<'static, Message> {
    let dot = text(if online { "●" } else { "○" })
        .size(10.0 * scale)
        .color(if online { palette::OK } else { palette::DIM });
    let icon: Element<'static, Message> = match avatar_thumb(avatar.as_deref(), 28.0 * scale) {
        Some(image) => image,
        None => dot.into(),
    };

    let has_unread = unread > 0;
    // to_owned: фрагмент текста должен владеть строкой (Element 'static
    // для lazy), &str протянул бы заимствование из Store. База 16 —
    // прежний дефолт iced, принятый в S1 вид строки не меняем.
    let name_limit = (20.0 / scale).round().clamp(12.0, 20.0) as usize;
    let name = text(display_name(name, name_limit))
        .size(16.0 * scale)
        .wrapping(iced::widget::text::Wrapping::None)
        .font(if has_unread || selected {
            palette::bold()
        } else {
            iced::Font::default()
        })
        .color(if has_unread || selected {
            palette::TEXT
        } else {
            palette::MUTED
        })
        .width(Length::Fill)
        .height(Length::Fixed(24.0 * scale));

    let mut cells = row![icon, name]
        .spacing(8)
        .height(Length::Fill)
        .align_y(iced::alignment::Vertical::Center);
    if has_unread {
        cells = cells.push(
            container(text(unread.to_string()).size(11.0 * scale))
                .padding(Padding {
                    top: 2.0,
                    right: 7.0,
                    bottom: 2.0,
                    left: 7.0,
                })
                .style(palette::badge),
        );
    }

    button(cells.padding(Padding {
        top: 0.0,
        right: 12.0,
        bottom: 0.0,
        left: 12.0,
    }))
    .on_press(Message::Chat(ChatIn::Select(cid)))
    .style(palette::chat_row(selected))
    .width(Length::Fill)
    .height(Length::Fixed(52.0))
    .into()
}

/// Разделитель дня по центру: «Сегодня», «Вчера», «12 марта».
pub fn date_sep(ts_ms: i64, scale: f32) -> Element<'static, Message> {
    container(
        text(crate::ui::date_label(ts_ms))
            .size(11.0 * scale)
            .color(palette::MUTED),
    )
    .width(Length::Fill)
    .align_x(Horizontal::Center)
    .padding(Padding {
        top: 10.0,
        right: 0.0,
        bottom: 6.0,
        left: 0.0,
    })
    .into()
}

pub fn history_note(body: String, scale: f32) -> Element<'static, Message> {
    container(text(body).size(12.0 * scale).color(palette::MUTED))
        .width(Length::Fill)
        .align_x(Horizontal::Center)
        .padding(12)
        .into()
}

pub fn history_error(body: String, scale: f32) -> Element<'static, Message> {
    container(
        column![
            text(format!("История не загрузилась: {body}"))
                .size(12.0 * scale)
                .color(palette::ERR),
            button(text("Повторить").size(12.0 * scale).color(palette::TEXT))
                .padding([6, 12])
                .style(palette::button_dim)
                .on_press(Message::Chat(ChatIn::RetryHistory)),
        ]
        .spacing(8)
        .align_x(Horizontal::Center),
    )
    .width(Length::Fill)
    .padding(12)
    .into()
}

/// Служебные строки используют нейтральный цвет; ошибки имеют отдельный стиль.
pub fn sys_line(body: String, scale: f32) -> Element<'static, Message> {
    container(text(body).size(11.0 * scale).color(palette::MUTED))
        .width(Length::Fill)
        .align_x(Horizontal::Center)
        .padding(Padding {
            top: 2.0,
            right: 0.0,
            bottom: 2.0,
            left: 0.0,
        })
        .into()
}

/// Статус отправки моей группы — подпись в подвале (честность статусов,
/// GUI_STYLE.md §2). У чужой группы статуса нет.
fn send_status(send: &SendState) -> Option<(String, iced::Color)> {
    match send {
        SendState::Sending => Some(("отправляю".to_owned(), palette::MUTED)),
        SendState::Sent => Some(("отправлено".to_owned(), palette::MUTED)),
        SendState::Queued => Some(("отправится позже".to_owned(), palette::MUTED)),
        SendState::Delivered => Some(("доставлено".to_owned(), palette::OK)),
        SendState::Failed(reason) => Some((format!("не ушло: {reason}"), palette::ERR)),
    }
}

/// Подвал группы: время и статус отправки (только у меня).
fn group_footer(
    ts_ms: i64,
    status: &Option<(String, iced::Color)>,
    scale: f32,
) -> iced::Element<'static, Message> {
    let mut foot = row![text(time_hm(ts_ms))
        .size(10.0 * scale)
        .color(palette::MUTED)]
    .spacing(4.0);
    if let Some((label, color)) = status {
        foot = foot.push(text(label.clone()).size(10.0 * scale).color(*color));
    }
    foot.into()
}

/// Детерминированный id строки сообщения: операция прокрутки (S16)
/// находит её в дереве по этому id после раскладки.
pub fn msg_target_id(id: MsgId) -> iced::widget::Id {
    iced::widget::Id::from(format!("jump:{}", id.0))
}

/// Операция «прокрутить ленту к сообщению» (S16). iced 0.14 не имеет
/// scroll-into-view: измеряем на кадре после раскладки — viewport ленты
/// и текущий offset (обратный вызов `scrollable`), положение строки
/// (`container` с id-меткой) — и отвечаем абсолютным offset'ом, при
/// котором строка оказывается у верха вьюпорта. Перемотку делает update
/// через `operation::scroll_to`.
pub struct MeasureJump {
    feed: iced::widget::Id,
    target: iced::widget::Id,
    viewport: Option<iced::Rectangle>,
    offset: Option<f32>,
    target_bounds: Option<iced::Rectangle>,
}

impl MeasureJump {
    pub fn new(feed: iced::widget::Id, target: iced::widget::Id) -> Self {
        Self {
            feed,
            target,
            viewport: None,
            offset: None,
            target_bounds: None,
        }
    }
}

impl iced::advanced::widget::Operation<Option<f32>> for MeasureJump {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<Option<f32>>),
    ) {
        operate(self);
    }

    fn container(&mut self, id: Option<&iced::widget::Id>, bounds: iced::Rectangle) {
        if id == Some(&self.target) {
            self.target_bounds = Some(bounds);
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&iced::widget::Id>,
        bounds: iced::Rectangle,
        _content_bounds: iced::Rectangle,
        translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::scrollable::Scrollable,
    ) {
        if id == Some(&self.feed) {
            self.viewport = Some(bounds);
            self.offset = Some(translation.y);
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<Option<f32>> {
        use iced::advanced::widget::operation::Outcome;
        // Координаты строки — оконные; позиция в контенте ленты: отступ
        // от верха вьюпорта плюс текущая прокрутка. None — строка не
        // нашлась в дереве (не должно случаться: цель загружена).
        Outcome::Some(match (self.viewport, self.offset, self.target_bounds) {
            (Some(viewport), Some(offset), Some(target)) => {
                let content_y = target.y - viewport.y + offset;
                // Небольшой воздух сверху, чтобы строка не липла к краю.
                Some((content_y - 8.0).max(0.0))
            }
            _ => None,
        })
    }
}

/// Тело сообщения с id-меткой и подсветкой цели перехода (S16).
fn target_row(
    inner: Element<'static, Message>,
    msg_id: MsgId,
    highlight: Option<MsgId>,
) -> Element<'static, Message> {
    let mut row = container(inner).id(msg_target_id(msg_id));
    if highlight == Some(msg_id) {
        row = row.style(palette::jump_highlight);
    }
    row.into()
}

/// Обёртка строки группы: ширина ленты, отступ, выравнивание.
fn wrap_row(
    inner: iced::Element<'static, Message>,
    align_x: Horizontal,
) -> Element<'static, Message> {
    container(inner)
        .width(Length::Fill)
        .align_x(align_x)
        .padding(Padding {
            top: 3.0,
            right: 2.0,
            bottom: 3.0,
            left: 2.0,
        })
        .into()
}

/// Группа сообщений одного автора: пузыри моей стороны справа, чужие
/// слева; тексты в пузыре — по левому краю; время и статус — строкой
/// под пузырём, у его правой кромки. Fill-распорок в цепочке группы
/// нет ни одной: они протягивают ширину ближайшего ограничителя и
/// распахивают пузырь (правка по замерам пробы раскладки).
pub fn message_group(
    group: &FeedItem,
    scale: f32,
    revealed: &HashSet<MsgId>,
    highlight: Option<MsgId>,
) -> Vec<Element<'static, Message>> {
    let FeedItem::Group {
        mine,
        name: _,
        ts_ms,
        bodies,
        send,
        ..
    } = group
    else {
        return Vec::new();
    };

    let align_x = if *mine {
        Horizontal::Right
    } else {
        Horizontal::Left
    };

    let status = if *mine { send_status(send) } else { None };

    let bubble_style: fn(&iced::Theme) -> iced::widget::container::Style = if *mine {
        palette::bubble_mine
    } else {
        palette::bubble_theirs
    };
    let bubble = |col: iced::widget::Column<'static, Message>| {
        container(col)
            .max_width(560)
            .padding(Padding {
                top: 7.0,
                right: 12.0,
                bottom: 7.0,
                left: 12.0,
            })
            .style(bubble_style)
    };

    let mut elements: Vec<Element<'static, Message>> = Vec::with_capacity(bodies.len());
    let mut pending: Option<iced::widget::Column<'static, Message>> = None;
    let count = bodies.len();

    for (index, fb) in bodies.iter().enumerate() {
        let (msg_id, body, entities, reactions) = (&fb.id, &fb.body, &fb.entities, &fb.reactions);
        let last = index + 1 == count;
        // ПКМ по строке тела — меню сообщения (закрепить/открепить, S12);
        // в слитом пузыре каждая строка адресует своё сообщение. Клик по
        // строке со спойлером раскрывает его (S14).
        // Файл (S10) рендерится своей строкой; текст — разметкой (S14).
        let body_text: Element<'static, Message> = match &fb.file {
            Some(file) => file_row(
                fb.id,
                file,
                scale,
                file_metadata_color(*mine, false, body.len()),
            ),
            None => {
                let mut area =
                    mouse_area(rich_body(body, entities, revealed.contains(msg_id), scale))
                        .on_right_press(Message::Chat(ChatIn::OpenMsgMenu(*msg_id)));
                if entities.iter().any(|e| e.kind == EntityKind::Spoiler) {
                    area = area.on_press(Message::Chat(ChatIn::ToggleSpoiler(*msg_id)));
                }
                area.into()
            }
        };
        // Id-метка для прокрутки к сообщению и подсветка цели (S16).
        let body_text = target_row(body_text, *msg_id, highlight);

        // Длинные тексты — без пузыря, на всю ширину (GUI_STYLE.md §2).
        if body.len() > 300 {
            if let Some(col) = pending.take() {
                elements.push(wrap_row(bubble(col).into(), align_x));
            }
            let mut block = iced::widget::Column::new().push(
                container(body_text)
                    .max_width(560)
                    .padding(Padding {
                        top: 3.0,
                        right: 4.0,
                        bottom: 3.0,
                        left: 4.0,
                    })
                    .style(palette::no_bubble),
            );
            if let Some(pills) = reaction_pills(*msg_id, reactions, scale) {
                block = block.push(pills);
            }
            if last {
                elements.push(wrap_row(
                    group_with_footer(block, group_footer(*ts_ms, &status, scale)),
                    align_x,
                ));
            } else {
                elements.push(wrap_row(block.into(), align_x));
            }
        } else {
            let mut col = pending
                .take()
                .unwrap_or_else(|| iced::widget::Column::new().spacing(2));
            col = col.push(body_text);
            // Пилюли реакций — в пузыре, под своим телом (S15).
            if let Some(pills) = reaction_pills(*msg_id, reactions, scale) {
                col = col.push(pills);
            }
            pending = Some(col);
        }
    }
    if let Some(col) = pending.take() {
        elements.push(wrap_row(
            group_with_footer(bubble(col), group_footer(*ts_ms, &status, scale)),
            align_x,
        ));
    }

    elements
}

/// Компактная группа: без пузырей, всё слева, мои строки — с отступом
/// (GUI_STYLE.md §2). Имя автора — над чужой группой: выравнивание
/// больше не отличает авторов.
pub fn compact_group(
    group: &FeedItem,
    scale: f32,
    revealed: &HashSet<MsgId>,
    highlight: Option<MsgId>,
) -> Vec<Element<'static, Message>> {
    let FeedItem::Group {
        mine,
        name,
        ts_ms,
        bodies,
        send,
        ..
    } = group
    else {
        return Vec::new();
    };

    let status = if *mine { send_status(send) } else { None };
    let mut col = iced::widget::Column::<Message>::new().spacing(2);
    if !*mine {
        col = col.push(
            text(name.clone())
                .size(12.0 * scale)
                .font(palette::bold())
                .color(palette::MUTED),
        );
    }
    let indent = if *mine { 16.0 } else { 0.0 };
    for b in bodies {
        let (msg_id, body, entities, reactions) = (&b.id, &b.body, &b.entities, &b.reactions);
        // ПКМ по строке — меню сообщения (закрепить/открепить, S12);
        // клик по спойлеру раскрывает (S14).
        let content: Element<'static, Message> = match &b.file {
            Some(file) => file_row(
                b.id,
                file,
                scale,
                file_metadata_color(*mine, true, body.len()),
            ),
            None => {
                let mut area =
                    mouse_area(rich_body(body, entities, revealed.contains(msg_id), scale))
                        .on_right_press(Message::Chat(ChatIn::OpenMsgMenu(*msg_id)));
                if entities.iter().any(|e| e.kind == EntityKind::Spoiler) {
                    area = area.on_press(Message::Chat(ChatIn::ToggleSpoiler(*msg_id)));
                }
                area.into()
            }
        };
        // Id-метка для прокрутки к сообщению и подсветка цели (S16).
        let content = target_row(content, *msg_id, highlight);
        col = col.push(container(content).padding(Padding {
            left: indent,
            ..Padding::default()
        }));
        // Пилюли реакций — под строкой тела (S15).
        if let Some(pills) = reaction_pills(*msg_id, reactions, scale) {
            col = col.push(container(pills).padding(Padding {
                left: indent,
                ..Padding::default()
            }));
        }
    }
    col = col.push(group_footer(*ts_ms, &status, scale));

    vec![wrap_row(col.into(), Horizontal::Left)]
}

/// Пузырь (или длинный блок) + подвал под ним: подвал прижат к правой
/// кромке самого широкого элемента группы — колонка Shrink, оба ребёнка
/// Shrink, align_x(Right) сводит их правые кромки.
fn group_with_footer(
    main: impl Into<iced::Element<'static, Message>>,
    footer: iced::Element<'static, Message>,
) -> iced::Element<'static, Message> {
    iced::widget::Column::new()
        .push(main.into())
        .push(container(footer).padding(Padding {
            top: 1.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        }))
        .align_x(Horizontal::Right)
        .into()
}

// ---------- оверлеи (GUI_UI.md §1, спринт S2) ----------

/// Глухая подложка без on_press (оверлей звонка, S7): клик мимо не
/// закрывает — Esc и крест во время активного звонка запрещены.
pub fn backdrop_dim() -> Element<'static, Message> {
    container(
        iced::widget::space::Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(palette::backdrop)
    .into()
}

/// Подложка оверлея: клик мимо панели закрывает её.
pub fn backdrop(on_close: Message) -> Element<'static, Message> {
    container(
        button(
            iced::widget::space::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(palette::button_invisible)
        .on_press(on_close),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(palette::backdrop)
    .into()
}

/// Инвайт-диалог: мой код (эллипсис + QR + копирование) и чужой код.
/// Возвращаемый Element заимствует view — оверлей не lazy.
pub fn invite_panel(view: &InviteView, scale: f32) -> Element<'_, Message> {
    let mut body = column![text("Приглашение")
        .size(14.0 * scale)
        .font(palette::bold())
        .color(palette::TEXT),]
    .spacing(10);

    // Форма выдачи: адрес опционален; relay только с адресом (CORE-API §4.2).
    body = body.push(text("Адрес").size(11.0 * scale).color(palette::MUTED));
    body = body.push(
        text_input("192.168.1.5:7777 — необязательно", &view.addr)
            .id(view.addr_id.clone())
            .on_input(|value| Message::Meet(MeetIn::AddrInput(value)))
            .size(12.0 * scale)
            .padding(8)
            .style(palette::input),
    );
    body = body.push(
        iced::widget::checkbox(view.relay)
            .label("через посредника — собеседник не увидит ваш IP")
            .on_toggle(|on| Message::Meet(MeetIn::RelayToggled(on)))
            .text_size(11.0 * scale)
            .style(palette::checkbox_style),
    );
    body = body.push(
        text("Без адреса код работает только в одной локальной сети.")
            .size(10.0 * scale)
            .color(palette::MUTED),
    );
    body = body.push(
        button(text("Выдать код").size(12.0 * scale).font(palette::bold()))
            .padding(Padding {
                top: 8.0,
                right: 14.0,
                bottom: 8.0,
                left: 14.0,
            })
            .style(palette::button_accent)
            .on_press_maybe((!view.submitting).then_some(Message::Meet(MeetIn::IssueInvite))),
    );

    body = body.push(text("Мой код").size(11.0 * scale).color(palette::MUTED));
    body = body.push(match (&view.code, &view.error) {
        (Some(code), _) => text(crate::ui::elided_code(code))
            .size(11.0 * scale)
            .color(palette::TEXT),
        (None, None) => text("Код появится после выдачи.")
            .size(11.0 * scale)
            .color(palette::MUTED),
        (None, Some(_)) => text("Код не выдан.").size(11.0 * scale).color(palette::ERR),
    });

    if view.code.is_some() {
        let copy = button(
            text("Скопировать код")
                .size(12.0 * scale)
                .font(palette::bold()),
        )
        .padding(Padding {
            top: 8.0,
            right: 14.0,
            bottom: 8.0,
            left: 14.0,
        })
        .style(palette::button_accent)
        .on_press(Message::Meet(MeetIn::CopyCode));

        let actions = match &view.qr {
            Some(data) => row![
                copy,
                space::Space::new().width(Length::Fill),
                qr_code::QRCode::new(data)
                    .total_size(120)
                    .style(palette::qr),
            ]
            .spacing(12)
            .align_y(iced::alignment::Vertical::Center),
            None => row![copy],
        };
        body = body.push(actions);
        body = body.push(
            text("Код одноразовый: действует до первого подключения.")
                .size(10.0 * scale)
                .color(palette::MUTED),
        );
    }

    body = body.push(space::Space::new().height(4));
    body = body
        .push(
            text("Вставить чужой код")
                .size(11.0 * scale)
                .color(palette::MUTED),
        )
        .push(
            row![
                text_input("Вставьте код приглашения", &view.foreign)
                    .on_input(|value| Message::Meet(MeetIn::ForeignInput(value)))
                    .on_submit(Message::Meet(MeetIn::SubmitForeign))
                    .size(13.0 * scale)
                    .padding(8)
                    .style(palette::input)
                    .width(Length::Fill),
                button(text("Добавить").size(12.0 * scale).font(palette::bold()))
                    .padding(Padding {
                        top: 8.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 14.0,
                    })
                    .style(palette::button_accent)
                    .on_press_maybe(
                        (!view.submitting && !view.foreign.trim().is_empty())
                            .then_some(Message::Meet(MeetIn::SubmitForeign)),
                    ),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
        );

    if let Some(error) = &view.error {
        body = body.push(text(error.clone()).size(11.0 * scale).color(palette::ERR));
    }

    container(body)
        .width(Length::Fixed(520.0))
        .padding(20)
        .style(palette::dialog)
        .into()
}

/// Карточка контакта: имя, состояние, блокировка (GUI_UI.md §1).
/// Отпечатка в API 1.0.0 нет (GET_IDENTITY — только свой) — карточка
/// показывает имя и действия.
pub fn contact_card(
    cid: Cid,
    name: &str,
    online: bool,
    blocked: bool,
    scale: f32,
    avatar: Option<Vec<u8>>,
) -> Element<'static, Message> {
    let (status, color) = if blocked {
        ("заблокирован", palette::ERR)
    } else if online {
        ("в сети", palette::OK)
    } else {
        ("не в сети", palette::DIM)
    };
    let block_label = if blocked {
        "Разблокировать"
    } else {
        "Заблокировать"
    };

    let mut head = column![].spacing(10);
    if let Some(image) = avatar_thumb(avatar.as_deref(), 64.0 * scale) {
        head = head.push(image);
    }
    container(
        column![
            head,
            text(name.to_owned())
                .size(15.0 * scale)
                .font(palette::bold())
                .color(palette::TEXT),
            text(status).size(12.0 * scale).color(color),
            space::Space::new().height(4),
            row![
                button(text(block_label).size(12.0 * scale).color(if blocked {
                    palette::TEXT
                } else {
                    palette::ERR
                }))
                .padding(Padding {
                    top: 8.0,
                    right: 14.0,
                    bottom: 8.0,
                    left: 14.0,
                })
                .style(palette::button_dim)
                .on_press(Message::Meet(MeetIn::ToggleBlock(cid))),
                space::Space::new().width(Length::Fill),
                button(text("Закрыть").size(12.0 * scale).color(palette::TEXT))
                    .padding(Padding {
                        top: 8.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 14.0,
                    })
                    .style(palette::button_plain)
                    .on_press(Message::Meet(MeetIn::CloseOverlay)),
            ]
            .align_y(iced::alignment::Vertical::Center),
        ]
        .spacing(10),
    )
    .width(Length::Fixed(420.0))
    .padding(16)
    .style(palette::dialog)
    .into()
}

/// Меню по правой кнопке на контакте: блок/разблок, информация и —
/// на ядре 2.2+ (`v22`) — закреп, отправка визитки, удаление чата
/// (GUI_UI.md §4).
pub fn context_menu(
    menu: &MenuView,
    name: &str,
    blocked: bool,
    pinned: bool,
    v22: bool,
    scale: f32,
) -> Element<'static, Message> {
    let label = if blocked {
        "Разблокировать"
    } else {
        "Заблокировать"
    };
    let mut items = column![
        text(name.to_owned())
            .size(11.0 * scale)
            .color(palette::MUTED),
        button(text(label).size(13.0 * scale).color(if blocked {
            palette::TEXT
        } else {
            palette::ERR
        }))
        .width(Length::Fill)
        .padding(Padding {
            top: 6.0,
            right: 10.0,
            bottom: 6.0,
            left: 10.0,
        })
        .style(palette::button_menu)
        .on_press(Message::Meet(MeetIn::ToggleBlock(menu.cid))),
        button(text("Информация").size(13.0 * scale).color(palette::TEXT))
            .width(Length::Fill)
            .padding(Padding {
                top: 6.0,
                right: 10.0,
                bottom: 6.0,
                left: 10.0,
            })
            .style(palette::button_menu)
            .on_press(Message::Meet(MeetIn::OpenCard(menu.cid))),
    ]
    .spacing(2);
    if v22 {
        let pin_label = if pinned {
            "Открепить чат"
        } else {
            "Закрепить чат"
        };
        items = items.push(
            button(text(pin_label).size(13.0 * scale).color(palette::TEXT))
                .width(Length::Fill)
                .padding(Padding {
                    top: 6.0,
                    right: 10.0,
                    bottom: 6.0,
                    left: 10.0,
                })
                .style(palette::button_menu)
                .on_press(Message::Meet(MeetIn::TogglePinChat(menu.cid))),
        );
        items = items.push(
            button(
                text("Отправить визитку…")
                    .size(13.0 * scale)
                    .color(palette::TEXT),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 6.0,
                right: 10.0,
                bottom: 6.0,
                left: 10.0,
            })
            .style(palette::button_menu)
            .on_press(Message::Meet(MeetIn::OpenSendCard(menu.cid))),
        );
        items = items.push(
            button(text("Удалить чат").size(13.0 * scale).color(palette::ERR))
                .width(Length::Fill)
                .padding(Padding {
                    top: 6.0,
                    right: 10.0,
                    bottom: 6.0,
                    left: 10.0,
                })
                .style(palette::button_menu)
                .on_press(Message::Meet(MeetIn::AskDeleteChat(menu.cid))),
        );
    }
    container(items)
        .width(Length::Fixed(200.0))
        .padding(8)
        .style(palette::dialog)
        .into()
}

/// Визитка третьего лица в ленте (S13): карточка знакомит, но не
/// подключает — добавить человека можно только его приглашением
/// (CORE-API §4.8), поэтому визитка не интерактивна.
pub fn card_bubble(name: String, scale: f32) -> Element<'static, Message> {
    container(
        column![
            text(name)
                .size(14.0 * scale)
                .font(palette::bold())
                .color(palette::TEXT),
            text("визитка · добавить можно по приглашению")
                .size(10.0 * scale)
                .color(palette::MUTED),
        ]
        .spacing(2),
    )
    .padding(Padding {
        top: 8.0,
        right: 14.0,
        bottom: 8.0,
        left: 14.0,
    })
    .style(palette::bubble_theirs)
    .into()
}

/// Выбор визитки для отправки в чат (S13): «Моя визитка» (ядро пошлёт
/// карточку профиля) или карточка другого контакта; чат-получатель
/// из списка исключён (view его отрезал).
pub fn send_card_panel(rows: Vec<(Option<Cid>, String)>, scale: f32) -> Element<'static, Message> {
    let mut list = column![text("Чья визитка?")
        .size(16.0 * scale)
        .font(palette::bold())
        .color(palette::TEXT)]
    .spacing(2);
    for (contact, name) in rows {
        list = list.push(
            button(text(name).size(13.0 * scale).color(palette::TEXT))
                .width(Length::Fill)
                .padding(Padding {
                    top: 6.0,
                    right: 10.0,
                    bottom: 6.0,
                    left: 10.0,
                })
                .style(palette::button_menu)
                .on_press(Message::Meet(MeetIn::PickSendCard(contact))),
        );
    }
    list = list.push(
        button(text("Отмена").size(13.0 * scale).color(palette::MUTED))
            .width(Length::Fill)
            .padding(Padding {
                top: 6.0,
                right: 10.0,
                bottom: 6.0,
                left: 10.0,
            })
            .style(palette::button_menu)
            .on_press(Message::Meet(MeetIn::CloseOverlay)),
    );
    container(scrollable(list))
        .width(Length::Fixed(320.0))
        .padding(16)
        .style(palette::dialog)
        .into()
}

/// Честное подтверждение удаления чата (S13, DELETE_CHAT): действие
/// необратимо и удаляет только нашу сторону — у собеседника всё останется.
pub fn confirm_delete_panel(cid: Cid, name: &str, scale: f32) -> Element<'static, Message> {
    container(
        column![
            text(format!("Удалить чат с {name}?"))
                .size(15.0 * scale)
                .font(palette::bold())
                .color(palette::TEXT),
            text(
                "История и контакт удалятся безвозвратно — только у вас. \
                 Написать снова можно будет по новому приглашению."
            )
            .size(12.0 * scale)
            .color(palette::MUTED),
            row![
                button(text("Удалить").size(12.0 * scale).color(palette::ERR))
                    .padding(Padding {
                        top: 8.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 14.0,
                    })
                    .style(palette::button_dim)
                    .on_press(Message::Meet(MeetIn::ConfirmDeleteChat(cid))),
                space::Space::new().width(Length::Fill),
                button(text("Отмена").size(12.0 * scale).color(palette::TEXT))
                    .padding(Padding {
                        top: 8.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 14.0,
                    })
                    .style(palette::button_plain)
                    .on_press(Message::Meet(MeetIn::CloseOverlay)),
            ]
            .align_y(iced::alignment::Vertical::Center),
        ]
        .spacing(10),
    )
    .width(Length::Fixed(420.0))
    .padding(16)
    .style(palette::dialog)
    .into()
}

/// Панель звонка (S7, GUI_UI.md §1, 420×260): входящий — принять/
/// отклонить; исходящий — отменить; активный — таймер, mute, завершить.
/// Кнопки — глаголы (TEXT_STYLE §3).
pub fn call_panel(
    view: &CallView,
    name: &str,
    now_ms: i64,
    scale: f32,
) -> Element<'static, Message> {
    let action = |label: &'static str, msg: Message, accent: bool| {
        let label = text(label).size(13.0 * scale);
        // У акцентной кнопки свой контрастный текст из стиля палитры.
        let label = if accent {
            label
        } else {
            label.color(palette::TEXT)
        };
        button(label)
            .padding(Padding {
                top: 8.0,
                right: 16.0,
                bottom: 8.0,
                left: 16.0,
            })
            .style(if accent {
                palette::button_accent
            } else {
                palette::button_dim
            })
            .on_press(msg)
    };

    let title = text(name.to_owned())
        .size(16.0 * scale)
        .font(palette::bold())
        .color(palette::TEXT);
    let body: iced::widget::Column<'_, Message> = match view.phase {
        CallPhase::Incoming => column![
            title,
            text("входящий звонок")
                .size(12.0 * scale)
                .color(palette::MUTED),
            row![
                action("Принять", Message::Call(CallIn::Answer), true),
                action("Отклонить", Message::Call(CallIn::Decline), false),
            ]
            .spacing(8),
        ]
        .spacing(12),
        CallPhase::Outgoing => column![
            title,
            text("звоним…").size(12.0 * scale).color(palette::MUTED),
            action("Отменить", Message::Call(CallIn::Decline), false),
        ]
        .spacing(12),
        CallPhase::Active => {
            let secs = ((now_ms - view.started_ms) / 1000).max(0);
            let timer = format!("{}:{:02}", secs / 60, secs % 60);
            let mute = if view.muted {
                "Включить микрофон"
            } else {
                "Выключить микрофон"
            };
            column![
                title,
                text(timer).size(20.0 * scale).color(palette::TEXT),
                row![
                    action(mute, Message::Call(CallIn::ToggleMute), false),
                    action("Завершить", Message::Call(CallIn::Hangup), false),
                ]
                .spacing(8),
            ]
            .spacing(12)
        }
    };

    container(body)
        .width(Length::Fixed(420.0))
        .height(Length::Fixed(260.0))
        .padding(20)
        .style(palette::dialog)
        .into()
}

/// Плашка закрепов под шапкой чата (S12): число и последнее тело;
/// клик открывает список. Рисуется, только когда закрепы есть.
pub fn pins_bar(count: usize, preview: Option<&str>, scale: f32) -> Element<'static, Message> {
    let label = match preview {
        Some(body) => format!("Закреплено: {count} · {body}"),
        None => format!("Закреплено: {count}"),
    };
    button(
        text(label)
            .size(11.0 * scale)
            .color(palette::MUTED)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 4.0,
        right: 16.0,
        bottom: 4.0,
        left: 16.0,
    })
    .style(palette::button_dim)
    .on_press(Message::Chat(ChatIn::OpenPins))
    .into()
}

/// Список закреплённых сообщений (S12): время, тело, «Открепить».
/// Записи без тела (GET_MESSAGE ещё в пути) не показываются. Клик по
/// строке ведёт к сообщению в ленте, если оно загружено (S16); иначе
/// строка без действия с честной пометкой. `loaded` — msg_id ядра,
/// которые уже есть в загруженной ленте чата.
pub fn pins_panel(
    entries: &[PinEntry],
    loaded: &HashSet<CoreMsgId>,
    cid: Cid,
    scale: f32,
) -> Element<'static, Message> {
    let mut list = column![text("Закреплённые сообщения")
        .size(16.0 * scale)
        .font(palette::bold())
        .color(palette::TEXT)]
    .spacing(6);
    let resolved = entries.iter().filter(|p| p.body.is_some()).count();
    for entry in entries {
        let Some(body) = &entry.body else { continue };
        let info = column![
            text(time_hm(entry.ts_ms))
                .size(10.0 * scale)
                .color(palette::MUTED),
            text(body.clone()).size(13.0 * scale).color(palette::TEXT),
        ]
        .spacing(1)
        .width(Length::Fill);
        let info: Element<'static, Message> = if loaded.contains(&entry.msg_id) {
            button(info)
                .width(Length::Fill)
                .padding(4)
                .style(palette::button_plain)
                .on_press(Message::Chat(ChatIn::JumpTo {
                    cid,
                    key: JumpKey::Core(entry.msg_id),
                }))
                .into()
        } else {
            column![
                info,
                text("ещё не загружено — поднимите ленту выше")
                    .size(10.0 * scale)
                    .color(palette::MUTED),
            ]
            .spacing(2)
            .into()
        };
        list = list.push(
            row![
                info,
                button(text("Открепить").size(11.0 * scale).color(palette::MUTED))
                    .padding(Padding {
                        top: 4.0,
                        right: 8.0,
                        bottom: 4.0,
                        left: 8.0,
                    })
                    .style(palette::button_dim)
                    .on_press(Message::Chat(ChatIn::UnpinMessage(entry.msg_id))),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
        );
    }
    let failed = entries
        .iter()
        .filter(|p| p.failed && p.body.is_none())
        .count();
    let loading = entries.len() - resolved - failed;
    if loading > 0 {
        list = list.push(
            text(format!("Загружается: {} из {}", resolved, entries.len()))
                .size(11.0 * scale)
                .color(palette::MUTED),
        );
    }
    if failed > 0 {
        list = list.push(
            text(format!("Не загрузилось: {failed}"))
                .size(11.0 * scale)
                .color(palette::MUTED),
        );
    }
    container(scrollable(list))
        .width(Length::Fixed(420.0))
        .padding(16)
        .style(palette::dialog)
        .into()
}

/// Меню сообщения по ПКМ (S12): закрепить/открепить. Пункт собирает
/// view (он знает, закреплено ли сообщение и есть ли у него msg_id ядра);
/// `pin = None` — своё сообщение в полёте: кнопка-глагол выключена,
/// под ней строка-пояснение (TEXT_STYLE §3: кнопка — глагол, объяснение
/// живёт отдельно).
/// Быстрые реакции меню (S15): кодпоинт = char, контракт §4.4.
/// «❤» без VS16 — ядро хранит один кодпоинт.
pub const QUICK_REACTIONS: [char; 6] = ['👍', '❤', '😂', '😮', '😢', '🔥'];

/// `pin`: None — пункта нет вовсе (ядро < 2.2); Some(None) — пункт
/// виден, но выключен (сообщение в полёте); Some(Some) — активен.
pub fn msg_menu(
    body: &str,
    pin: Option<Option<(&'static str, Message)>>,
    react: Option<MsgId>,
    scale: f32,
) -> Element<'static, Message> {
    let preview: String = body.chars().take(80).collect();
    let has_pin_row = pin.is_some();
    let can_pin = matches!(pin, Some(Some(_)));
    let (pin_label, pin_action) = match pin {
        Some(Some((label, message))) => (label, Some(message)),
        _ => ("Закрепить сообщение", None),
    };
    let mut pin_button = button(text(pin_label).size(13.0 * scale).color(
        if pin_action.is_some() {
            palette::TEXT
        } else {
            palette::MUTED
        },
    ))
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        right: 10.0,
        bottom: 6.0,
        left: 10.0,
    })
    .style(palette::button_menu);
    if let Some(message) = pin_action {
        pin_button = pin_button.on_press(message);
    }
    let mut items = column![text(preview).size(11.0 * scale).color(palette::MUTED)].spacing(2);
    if has_pin_row {
        items = items.push(pin_button);
    }
    if has_pin_row && !can_pin {
        items = items.push(
            text("Закрепить можно после отправки")
                .size(10.0 * scale)
                .color(palette::MUTED),
        );
    }
    // Ряд быстрых реакций (S15). Своё сообщение в полёте (нет msg_id
    // ядра) — ряд блёклый, без нажатий.
    let mut emojis = iced::widget::Row::new().spacing(2);
    for emoji in QUICK_REACTIONS {
        let mut b = button(
            text(emoji.to_string())
                .size(16.0 * scale)
                .font(palette::emoji())
                .color(if react.is_some() {
                    palette::TEXT
                } else {
                    palette::MUTED
                }),
        )
        .padding(Padding {
            top: 4.0,
            right: 6.0,
            bottom: 4.0,
            left: 6.0,
        })
        .style(palette::button_menu);
        if let Some(id) = react {
            b = b.on_press(Message::Chat(ChatIn::React(id, emoji)));
        }
        emojis = emojis.push(b);
    }
    items = items.push(emojis);
    if react.is_none() {
        items = items.push(
            text("Реагировать можно после отправки")
                .size(10.0 * scale)
                .color(palette::MUTED),
        );
    }
    items = items.push(
        button(text("Отмена").size(13.0 * scale).color(palette::MUTED))
            .width(Length::Fill)
            .padding(Padding {
                top: 6.0,
                right: 10.0,
                bottom: 6.0,
                left: 10.0,
            })
            .style(palette::button_menu)
            .on_press(Message::Meet(MeetIn::CloseOverlay)),
    );
    container(items)
        .width(Length::Fixed(220.0))
        .padding(8)
        .style(palette::dialog)
        .into()
}

/// Панель настроек: приватность уведомлений, компактная лента, масштаб
/// шрифта (S3, GUI_UI.md §4) и имя профиля (SET_NAME, S13.1). Возвращает
/// Element, заимствующий settings — значения читаются на каждый кадр.
pub fn settings_panel<'a>(
    settings: &'a Settings,
    view: &'a SettingsView,
    profile_name: Option<&'a str>,
    scale: f32,
    own_avatar: Option<&'a [u8]>,
) -> Element<'a, Message> {
    use crate::model::settings::{FONT_SCALE_MAX, FONT_SCALE_MIN, FONT_SCALE_STEP};

    let percent = (settings.font_scale * 100.0).round() as i32;
    let step_button = |label: &'static str, delta: f32, enabled: bool| {
        button(text(label).size(12.0 * scale).color(palette::TEXT))
            .padding(Padding {
                top: 6.0,
                right: 14.0,
                bottom: 6.0,
                left: 14.0,
            })
            .style(palette::button_dim)
            .on_press_maybe(
                enabled.then_some(Message::Settings(SettingsIn::ChangeFontScale(delta))),
            )
    };

    // Ядро может подставить собственный default, но GUI не обещает его.
    let name_len = view.name_input.trim().chars().count();
    let name_ok = (1..=32).contains(&name_len);
    let name_note = view.note.clone().or_else(|| {
        Some(match profile_name {
            Some(name) => format!("Сейчас задано: {name}"),
            None => "Имя не задано".to_owned(),
        })
    });

    container(
        column![
            text("Настройки")
                .size(14.0 * scale)
                .font(palette::bold())
                .color(palette::TEXT),
            text("Имя в визитках")
                .size(12.0 * scale)
                .color(palette::TEXT),
            row![
                text_input("Как вас видят получатели визитки", &view.name_input)
                    .id(view.input_id.clone())
                    .on_input(|value| Message::Settings(SettingsIn::NameInput(value)))
                    .on_submit(Message::Settings(SettingsIn::SaveName))
                    .size(13.0 * scale)
                    .padding(8)
                    .style(palette::input)
                    .width(Length::Fill),
                button(text("Сохранить").size(12.0 * scale).font(palette::bold()))
                    .padding(Padding {
                        top: 8.0,
                        right: 14.0,
                        bottom: 8.0,
                        left: 14.0,
                    })
                    .style(palette::button_accent)
                    .on_press_maybe(name_ok.then_some(Message::Settings(SettingsIn::SaveName))),
            ]
            .spacing(8),
            text(name_note.unwrap_or_default())
                .size(10.0 * scale)
                .color(palette::MUTED),
            // Аватарка (S8б): своё фото в визитках; 96×96 JPEG ≤ 16 КБ —
            // лимит ядра, сжатие при выборе.
            text("Фото в визитках")
                .size(12.0 * scale)
                .color(palette::TEXT),
            {
                let mut avatar_row = row![]
                    .spacing(10)
                    .align_y(iced::alignment::Vertical::Center);
                if let Some(image) = avatar_thumb(own_avatar, 48.0 * scale) {
                    avatar_row = avatar_row.push(image);
                }
                avatar_row.push(
                    button(text("Выбрать фото").size(12.0 * scale).color(palette::TEXT))
                        .padding(Padding {
                            top: 6.0,
                            right: 14.0,
                            bottom: 6.0,
                            left: 14.0,
                        })
                        .style(palette::button_dim)
                        .on_press(Message::Settings(SettingsIn::PickAvatar)),
                )
            },
            iced::widget::checkbox(settings.hide_notification_text)
                .label("Скрывать текст в уведомлениях")
                .on_toggle(|on| Message::Settings(SettingsIn::ToggleNotificationText(on)))
                .text_size(12.0 * scale)
                .style(palette::checkbox_style),
            iced::widget::checkbox(settings.compact)
                .label("Компактная лента — без пузырей, всё слева")
                .on_toggle(|on| Message::Settings(SettingsIn::ToggleCompact(on)))
                .text_size(12.0 * scale)
                .style(palette::checkbox_style),
            text(format!("Масштаб шрифта: {percent}%"))
                .size(12.0 * scale)
                .color(palette::MUTED),
            row![
                step_button(
                    "Уменьшить",
                    -FONT_SCALE_STEP,
                    settings.font_scale > FONT_SCALE_MIN
                ),
                step_button(
                    "Увеличить",
                    FONT_SCALE_STEP,
                    settings.font_scale < FONT_SCALE_MAX
                ),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(420.0))
    .padding(16)
    .style(palette::dialog)
    .into()
}

/// Сколько совпадений показывать в локальном режиме; остаток — строкой-хвостом.
const SEARCH_SHOW: usize = 50;

/// Куда ведёт клик по совпадению или закрепу (S16): локальный id уже
/// загруженного сообщения или msg_id ядра (решается в update — лента
/// могла подгрузить окно после сборки строк).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpKey {
    Local(MsgId),
    Core(CoreMsgId),
}

/// Строка совпадения панели поиска: ядерного (S11) или локального (S6).
pub struct SearchRow {
    pub ts_ms: i64,
    pub mine: bool,
    pub body: String,
    /// Имя чата — только глобальный поиск (S11).
    pub chat: Option<String>,
    /// Цель перехода: чат + ключ сообщения.
    pub jump: (Cid, JumpKey),
}

/// Панель Ctrl+F (S6/S11): запрос + совпадения. Ядерный режим (API 2.1+)
/// ищет по всей истории через демон: постраничная догрузка и скоп «все
/// чаты»; локальный (ядро 1.x) — по загруженной ленте. Клик по совпадению
/// ведёт к сообщению, если оно уже в загруженной ленте (S16); иначе
/// панель остаётся с честным пояснением.
pub fn search_panel<'a>(
    view: &'a SearchView,
    kernel: bool,
    searching: bool,
    rows: Vec<SearchRow>,
    scale: f32,
) -> Element<'a, Message> {
    let input = text_input("Что найти…", &view.query)
        .id(view.input_id.clone())
        .size(13.0 * scale)
        .padding(Padding {
            top: 8.0,
            right: 10.0,
            bottom: 8.0,
            left: 10.0,
        })
        .on_input(|text| Message::Chat(ChatIn::SearchInput(text)))
        .style(palette::input);

    let mut results = column![].spacing(2);
    if view.query.trim().is_empty() {
        let hint = if kernel {
            "Поиск по всей истории чата."
        } else {
            "Поиск ищет по загруженным сообщениям этого чата."
        };
        results = results.push(text(hint).size(12.0 * scale).color(palette::MUTED));
    } else if let Some(error) = &view.error {
        results = results.push(text(error.clone()).size(12.0 * scale).color(palette::ERR));
    } else if searching {
        results = results.push(text("Ищу…").size(12.0 * scale).color(palette::MUTED));
    } else if rows.is_empty() {
        results = results.push(
            text("Ничего не найдено.")
                .size(12.0 * scale)
                .color(palette::MUTED),
        );
    } else {
        let shown = if kernel {
            rows.len()
        } else {
            rows.len().min(SEARCH_SHOW)
        };
        for hit in rows.iter().take(shown) {
            let author = if hit.mine { "я" } else { "не я" };
            let mut line = row![text(time_hm(hit.ts_ms))
                .size(11.0 * scale)
                .color(palette::MUTED),]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center);
            if let Some(chat) = &hit.chat {
                line = line.push(text(chat.clone()).size(11.0 * scale).color(palette::MUTED));
            }
            line = line
                .push(
                    text(hit.body.clone())
                        .size(12.0 * scale)
                        .color(palette::TEXT),
                )
                .push(space::Space::new().width(Length::Fill))
                .push(text(author).size(11.0 * scale).color(palette::MUTED));
            results = results.push(
                button(line)
                    .width(Length::Fill)
                    .padding(Padding {
                        top: 4.0,
                        right: 8.0,
                        bottom: 4.0,
                        left: 8.0,
                    })
                    .style(palette::button_plain)
                    .on_press(Message::Chat(ChatIn::JumpTo {
                        cid: hit.jump.0,
                        key: hit.jump.1,
                    })),
            );
        }
        if kernel {
            if (rows.len() as u32) < view.total {
                results = results.push(
                    button(
                        text(format!("Показать ещё ({} из {})", rows.len(), view.total))
                            .size(12.0 * scale)
                            .color(palette::ACCENT),
                    )
                    .padding(Padding {
                        top: 4.0,
                        right: 8.0,
                        bottom: 4.0,
                        left: 8.0,
                    })
                    .style(palette::button_plain)
                    .on_press(Message::Chat(ChatIn::SearchMore)),
                );
            }
        } else if rows.len() > SEARCH_SHOW {
            results = results.push(
                text(format!("и ещё {}", rows.len() - SEARCH_SHOW))
                    .size(11.0 * scale)
                    .color(palette::MUTED),
            );
        }
    }

    let mut content = column![
        text("Поиск по чату")
            .size(14.0 * scale)
            .font(palette::bold())
            .color(palette::TEXT),
        input,
    ]
    .spacing(10);
    if kernel {
        content = content.push(
            checkbox(view.global)
                .label("Искать по всем чатам")
                .size(16.0 * scale)
                .text_size(12.0 * scale)
                .on_toggle(|on| Message::Chat(ChatIn::SearchScope(on))),
        );
    }

    container(content.push(scrollable(results).height(Length::Fixed(320.0))))
        .width(Length::Fixed(520.0))
        .padding(16)
        .style(palette::dialog)
        .into()
}

// ---------- файлы (S10) ----------

/// Строка файла в пузыре (S10): «📎 имя» + размер; клик открывает
/// сохранённый файл (xdg-open), ПКМ — меню сообщения. Без пути
/// (входящий на ядре без файла?) клика нет.
fn file_row(
    msg_id: MsgId,
    file: &crate::model::msg::FileRef,
    scale: f32,
    metadata_color: iced::Color,
) -> Element<'static, Message> {
    let content = iced::widget::Column::new()
        .push(
            text(format!("📎 {}", file.name))
                .size(14.0 * scale)
                .font(palette::emoji())
                .color(palette::TEXT),
        )
        .push(
            text(crate::ui::human_size(file.size))
                .size(11.0 * scale)
                .color(metadata_color),
        )
        .spacing(2);
    let mut area = mouse_area(content).on_right_press(Message::Chat(ChatIn::OpenMsgMenu(msg_id)));
    if let Some(path) = &file.path {
        area = area.on_press(Message::Chat(ChatIn::OpenLink(path.clone())));
    }
    area.into()
}

// ---------- реакции (S15) ----------

/// Пилюли реакций под телом: эмодзи + счётчик (>1); своя — акцентная.
/// Клик — переключить свою (React: SET, повторный — REMOVE).
fn reaction_pills(
    msg_id: MsgId,
    reactions: &[Reaction],
    scale: f32,
) -> Option<Element<'static, Message>> {
    if reactions.is_empty() {
        return None;
    }
    let mut row = iced::widget::Row::new().spacing(4);
    for r in reactions {
        let label = if r.count > 1 {
            format!("{} {}", r.emoji, r.count)
        } else {
            r.emoji.to_string()
        };
        // Пилюля на фоне пузыря: button_dim (RAISE) сливается с
        // пузырём — свой стиль на LINE; моя реакция — акцентный контур.
        row = row.push(
            button(
                text(label)
                    .size(11.0 * scale)
                    .font(palette::emoji())
                    .color(if r.mine {
                        palette::ACCENT
                    } else {
                        palette::TEXT
                    }),
            )
            .padding(Padding {
                top: 1.0,
                right: 6.0,
                bottom: 1.0,
                left: 6.0,
            })
            .style(palette::reaction_pill(r.mine))
            .on_press(Message::Chat(ChatIn::React(msg_id, r.emoji))),
        );
    }
    Some(row.into())
}

// ---------- разметка сообщений (S14, API 2.0+) ----------

/// Тело сообщения с разметкой: entities ядра → spans rich_text. Без
/// разметки — обычный текст (та же цена рендера). Спойлер до клика —
/// сплошная полоса (прозрачный текст на плашке). Ссылки кликабельны
/// (OpenLink → браузер ОС).
fn rich_body(
    body: &str,
    entities: &[Entity],
    revealed: bool,
    scale: f32,
) -> Element<'static, Message> {
    if entities.is_empty() {
        return text(body.to_owned())
            .size(14.0 * scale)
            .color(palette::TEXT)
            .into();
    }
    rich_text(body_spans(body, entities, revealed))
        .size(14.0 * scale)
        .on_link_click(|url| Message::Chat(ChatIn::OpenLink(url)))
        .into()
}

/// Разбить тело на сегменты по границам entities (пересечения сливаются
/// в общий стиль сегмента). Проводу не доверяем: сущности за пределами
/// тела или не по границам символов выбрасываются (ядро не валидирует
/// UTF-8 — резка по символам на клиенте, §4.3).
fn body_spans(body: &str, entities: &[Entity], revealed: bool) -> Vec<Span<'static, String>> {
    let valid: Vec<&Entity> = entities
        .iter()
        .filter(|e| {
            let start = usize::from(e.offset);
            let end = start + usize::from(e.len);
            e.len > 0
                && end <= body.len()
                && body.is_char_boundary(start)
                && body.is_char_boundary(end)
        })
        .collect();
    let mut bounds: BTreeSet<usize> = [0, body.len()].into_iter().collect();
    for e in &valid {
        bounds.insert(usize::from(e.offset));
        bounds.insert(usize::from(e.offset) + usize::from(e.len));
    }
    let bounds: Vec<usize> = bounds.into_iter().collect();

    let mut spans = Vec::new();
    for window in bounds.windows(2) {
        let (start, end) = (window[0], window[1]);
        let slice = &body[start..end];
        let covering: Vec<&&Entity> = valid
            .iter()
            .filter(|e| {
                usize::from(e.offset) <= start && end <= usize::from(e.offset) + usize::from(e.len)
            })
            .collect();

        // Скрытый спойлер: текст есть (копируется, ищется), но не читается.
        if covering.iter().any(|e| e.kind == EntityKind::Spoiler) && !revealed {
            spans.push(
                Span::new(slice.to_owned())
                    .color(iced::Color::from_rgba8(0, 0, 0, 0.0))
                    .background(palette::LINE),
            );
            continue;
        }

        let mut font = Font::DEFAULT;
        let mut color = palette::TEXT;
        let mut underline = false;
        let mut strike = false;
        let mut link: Option<String> = None;
        for e in covering {
            match e.kind {
                EntityKind::Bold => font.weight = Weight::Bold,
                EntityKind::Italic => font.style = FontStyle::Italic,
                EntityKind::Underline => underline = true,
                EntityKind::Strike => strike = true,
                EntityKind::Code | EntityKind::CodeBlock => font = Font::MONOSPACE,
                EntityKind::Link => {
                    color = palette::ACCENT;
                    underline = true;
                    link = Some(e.value.clone());
                }
                EntityKind::Spoiler => {} // раскрыт — обычный текст
            }
        }
        let mut span = Span::new(slice.to_owned())
            .font(font)
            .color(color)
            .underline(underline)
            .strikethrough(strike);
        if let Some(url) = link {
            span = span.link(url);
        }
        spans.push(span);
    }
    spans
}

fn file_metadata_color(mine: bool, compact: bool, body_len: usize) -> iced::Color {
    if mine && !compact && body_len <= 300 {
        palette::ACCENT_METADATA
    } else {
        palette::MUTED
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name, file_metadata_color};
    use crate::ui::palette;

    #[test]
    fn display_name_shortens_at_char_boundaries() {
        assert_eq!(display_name("short", 8), "short");
        assert_eq!(display_name("Длинное имя", 7), "Длинно…");
        assert_eq!(display_name("Жё/Шё", 4), "Жё/…");
        assert_eq!(display_name("имя", 0), "…");
    }

    #[test]
    fn file_metadata_color_follows_rendered_surface() {
        assert_eq!(file_metadata_color(false, false, 8), palette::MUTED);
        assert_eq!(
            file_metadata_color(true, false, 8),
            palette::ACCENT_METADATA
        );
        assert_eq!(file_metadata_color(true, true, 8), palette::MUTED);
        assert_eq!(file_metadata_color(true, false, 301), palette::MUTED);
    }
}
