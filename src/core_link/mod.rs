//! Канал с ядром: [`CoreLink`], типизированные запросы, ответы, события.
//!
//! Контракт — бинарный протокол CORE-API ядра stille-messenger (подмодуль `core/`
//! на 83ea91e, спецификация `core/zig/docs/CORE-API.md`; клиент совместим
//! с ядрами 1.x через зонд GET_VERSION): один REQUEST = один RESPONSE
//! с корреляцией по request_id, события рассылаются всем. Кодек кадров —
//! в [`api`], транспорт и раскладки payload — в [`uds`], демо-режим — в
//! [`mock`]. Строк на проводе нет: пользовательский текст появляется
//! только после [`LinkError::user_text`].

pub mod api;
pub mod mock;
pub mod uds;

use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use futures_core::Stream;

use crate::model::cid::Cid;
use crate::model::msg::CoreMsgId;
use crate::model::store::Contact;

/// Текущее время в мс — единая точка для ts у событий и сообщений.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Потолок окна GET_HISTORY на стороне ядра; больше — TOO_BIG
/// (CORE-API.md §4.3). Пагинация GUI выше этого окна не поднимается.
pub const HISTORY_MAX_LIMIT: u32 = 200;

/// Команда ядру; одна команда = один метод API 1.0.0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Список контактов (LIST_CONTACTS).
    Contacts,
    /// Отправить сообщение (SEND_MESSAGE). Офлайн-контакт — не ошибка:
    /// ответ `queued` означает outbox ядра. Entities — разметка тела
    /// (API 2.0+, §4.3); на ядрах 1.x tail не кодируется, список должен
    /// быть пуст (ui гейтит ввод разметки по версии).
    SendMsg {
        cid: Cid,
        text: String,
        entities: Vec<Entity>,
    },
    /// Поставить реакцию на сообщение (SET_REACTION, §4.4). Эмодзи —
    /// кодпоинт Unicode; чужие правила (живая сессия, контакт известен)
    /// проверяет ядро. Снятие удалением с той же парой (cid, msg_id).
    SetReaction {
        cid: Cid,
        msg_id: crate::model::msg::CoreMsgId,
        emoji: char,
    },
    /// Снять свою реакцию (REMOVE_REACTION).
    RemoveReaction {
        cid: Cid,
        msg_id: crate::model::msg::CoreMsgId,
        emoji: char,
    },
    /// Добавить контакт по инвайт-коду (ACCEPT_INVITE).
    Add { code: String },
    /// Выдать свой инвайт-код (CREATE_INVITE). Пустой addr — код для одной
    /// локальной сети (ядро ждёт LAN-анонс); с адресом — ядро дозванивается
    /// сразу; relay — адрес трактуется как релей, peer IP не видит.
    Invite { relay: bool, addr: String },
    /// Своя аватарка (SET_AVATAR, §4.4): байты картинки как есть,
    /// 1..=16000 Б. Ответ — avatar_id (SHA-256[0..16], content
    /// addressing) и размер.
    SetAvatar { data: Vec<u8> },
    /// avatar_id контакта (GET_AVATAR_ID, §4.4): нули — аватарки нет.
    /// Байты приходят сами событием AVATAR_RECEIVED (ядро запрашивает
    /// их у пира при рассинхроне и лечится на переподключении).
    GetAvatarId { cid: Cid },
    /// Отправить файл (SEND_FILE, §4.5): путь в ФС пользователя демона
    /// (демон тот же хост — читает сам). Имя — basename, ≤ 64 байт;
    /// лимит 64 МБ. Офлайн — не ошибка: outbox, уйдёт с сессией.
    SendFile { cid: Cid, path: String },
    /// Заблокировать контакт (BLOCK_CONTACT).
    Block { cid: Cid },
    /// Разблокировать контакт (UNBLOCK_CONTACT).
    Unblock { cid: Cid },
    /// Версия API (GET_VERSION): readiness probe из CORE-API.md, первый
    /// запрос каждого соединения. От major зависят раскладки tail-зависимых
    /// методов (SEND_MESSAGE, GET_HISTORY).
    Version,
    /// История чата (GET_HISTORY): окно новейших записей, старые → новые.
    /// Лимит ядра — 200, 0 означает «дефолтные 50».
    History { cid: Cid, limit: u32 },
    /// Поиск по истории (API 2.1+, CORE-API §4.3): `cid = Some` — один чат
    /// (SEARCH_MESSAGE), `cid = None` — все чаты (GLOBAL_SEARCH_MESSAGE).
    /// Постранично: `limit` ≤ 200 (0 → дефолт 50), `offset` — пропуск
    /// совпадений. Ядра 1.x метод не знают — клиент обязан отгейтить
    /// по GET_VERSION и молча уйти в локальный поиск.
    Search {
        cid: Option<Cid>,
        query: String,
        limit: u32,
        offset: u32,
    },
    /// Закрепить чат (PIN_CHAT, API 2.2+): закреп один на демон,
    /// повторное закрепление идемпотентно (§4.8).
    PinChat { cid: Cid },
    /// Открепить чат (UNPIN_CHAT).
    UnpinChat,
    /// Какой чат закреплён (GET_PINNED_CHAT).
    PinnedChat,
    /// Закреплённые сообщения чата (GET_PINNED_MESSAGES): ≤16 msg_id
    /// в порядке закрепления (§4.8).
    PinnedMessages { cid: Cid },
    /// Закрепить сообщение (PIN_MESSAGE): сообщение обязано быть
    /// в истории чата, иначе NOT_FOUND.
    PinMessage { cid: Cid, msg_id: CoreMsgId },
    /// Открепить сообщение (UNPIN_MESSAGE, идемпотентен).
    UnpinMessage { cid: Cid, msg_id: CoreMsgId },
    /// Запись истории по msg_id (GET_MESSAGE) — тела закреплённых
    /// (GET_PINNED_MESSAGES отдаёт только идентификаторы).
    GetMessage { msg_id: CoreMsgId },
    /// Удалить чат и контакт безвозвратно (DELETE_CHAT, API 2.2+, §4.8):
    /// история, закрепы и очередь чата чистятся у ядра; повторное
    /// знакомство с тем же человеком — новый cid с чистой историей.
    DeleteChat { cid: Cid },
    /// Переслать визитку (SEND_CONTACT, API 2.2+, §4.8): `to` — чат-
    /// получатель, `contact` — чья карточка (None = моя, на проводе 0).
    /// Карточка знакомит, но не подключает: добавить человека можно
    /// только его собственным приглашением. Ошибки: NO_CONTACT,
    /// CONTACT_BLOCKED, NO_SESSION.
    SendContact { to: Cid, contact: Option<Cid> },
    /// Задать имя профиля (SET_NAME, §4.4): 1–32 символа. Ядро рассылает
    /// его пирам в PROFILE и кладёт в свою визитку (SEND_CONTACT со своей
    /// карточкой); обратно прочитать имя нельзя — запроса GET_NAME нет.
    SetName { name: String },
    /// Начать звонок (START_CALL, §4.6): линия одна на демон — занято
    /// (BUSY), нет контакта (NO_CONTACT) или живого соединения
    /// (NO_SESSION) — честная ошибка.
    StartCall { cid: Cid },
    /// Принять входящий звонок (ANSWER_CALL, §4.6). Без звонка — NO_CALL.
    AnswerCall,
    /// Завершить или отклонить звонок (END_CALL, §4.6). Без звонка —
    /// NO_CALL.
    EndCall,
}

/// Совпадение поиска ядра (SEARCH_MESSAGE/GLOBAL_SEARCH_MESSAGE, §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Чат, где найдено (у чатового поиска — тот же cid).
    pub cid: Cid,
    pub mine: bool,
    /// Пир подтвердил ACK (бит1 flags). Осмысленно только для своих.
    pub delivered: bool,
    /// Нечёткое совпадение (бит2 flags): идут после точных.
    pub fuzzy: bool,
    pub msg_id: CoreMsgId,
    pub ts_ms: i64,
    pub body: String,
    /// Разметка тела (API 2.0+).
    pub entities: Vec<Entity>,
}

/// Вид разметки (CORE-API.md §4.3, kind u8 на проводе). Неизвестный kind
/// из будущего ядра при разборе пропускается — рендерить всё равно нечего.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Bold,
    Italic,
    Underline,
    Strike,
    Spoiler,
    Code,
    CodeBlock,
    Link,
}

impl EntityKind {
    /// kind с провода (§4.3): None — вид из будущего ядра, пропускаем.
    pub fn from_wire(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Bold,
            2 => Self::Italic,
            3 => Self::Underline,
            4 => Self::Strike,
            5 => Self::Spoiler,
            6 => Self::Code,
            7 => Self::CodeBlock,
            8 => Self::Link,
            _ => return None,
        })
    }

    /// kind на провод.
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Bold => 1,
            Self::Italic => 2,
            Self::Underline => 3,
            Self::Strike => 4,
            Self::Spoiler => 5,
            Self::Code => 6,
            Self::CodeBlock => 7,
            Self::Link => 8,
        }
    }
}

/// Сущность разметки тела: offset/len — БАЙТЫ UTF-8 (§4.3), value —
/// URL у ссылки (обязателен) или язык у блока кода (опционален), у
/// остальных пуст.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub kind: EntityKind,
    pub offset: u16,
    pub len: u16,
    pub value: String,
}

impl Entity {
    /// Короткая запись для тестов и ui-парсера.
    pub fn new(kind: EntityKind, offset: u16, len: u16) -> Self {
        Self {
            kind,
            offset,
            len,
            value: String::new(),
        }
    }

    /// Ссылка с URL (§4.3: value обязателен, ≤ 255 байт).
    pub fn link(offset: u16, len: u16, url: String) -> Self {
        Self {
            kind: EntityKind::Link,
            offset,
            len,
            value: url,
        }
    }
}

/// Правила ядра к хвосту entities (§4.3): ≤ 32 записей, len ≥ 1,
/// offset + len ≤ длины тела в байтах, записи отсортированы по offset
/// (пересечения разрешены), value по kind (URL у ссылки обязателен и
/// ≤ 255 байт, язык блока кода опционален, у остальных пуст). Нарушение
/// ядро отвечает BAD_ARGS — проверяем до отправки и в заглушке.
pub fn entities_valid(body: &str, entities: &[Entity]) -> Result<(), &'static str> {
    if entities.len() > 32 {
        return Err("больше 32 сущностей");
    }
    let body_len = body.len();
    let mut prev_offset = 0u16;
    for (index, entity) in entities.iter().enumerate() {
        if entity.len == 0 {
            return Err("len = 0");
        }
        if usize::from(entity.offset) + usize::from(entity.len) > body_len {
            return Err("сущность за границей тела");
        }
        if index > 0 && entity.offset < prev_offset {
            return Err("сущности не отсортированы по offset");
        }
        prev_offset = entity.offset;
        match entity.kind {
            EntityKind::Link if entity.value.is_empty() => {
                return Err("ссылка без URL");
            }
            EntityKind::Link if entity.value.len() > 255 => {
                return Err("URL длиннее 255 байт");
            }
            EntityKind::CodeBlock => {} // язык опционален
            EntityKind::Link => {}
            _ if !entity.value.is_empty() => {
                return Err("value у простого вида разметки");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Ошибка канала с ядром (GUI_DEV.md §2: типизированные варианты).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// Демон не отвечает или соединение оборвалось.
    Disconnected,
    /// Ядро ответило машинным кодом статуса.
    Kernel(api::Status),
    /// Ответ не разобран — нарушение контракта со стороны ядра.
    BadFrame(&'static str),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "нет связи с ядром"),
            Self::Kernel(status) => write!(f, "ядро: {}", status.name()),
            Self::BadFrame(what) => write!(f, "кадр не разобран: {what}"),
        }
    }
}

impl std::error::Error for LinkError {}

impl LinkError {
    /// Текст для пользователя: что случилось + что делать (TEXT_STYLE §3).
    /// Ядро шлёт только машинные коды — человеческий текст рождается здесь.
    pub fn user_text(&self) -> String {
        match self {
            Self::Disconnected => "Нет соединения. Перезапустите приложение.".into(),
            Self::Kernel(api::Status::NoContact) => "Контакт не найден.".into(),
            Self::Kernel(api::Status::ContactBlocked) => {
                "Контакт заблокирован. Разблокируйте, чтобы писать.".into()
            }
            Self::Kernel(api::Status::BadInviteCode) => {
                "Неверный код. Скопируйте строку целиком — она начинается с stille://i/. Старые shopot://i/ тоже принимаются.".into()
            }
            // Реестр живых приглашений — память демона (§4.2): перезапуск
            // приложения его не очистит, остаётся ждать использования кодов.
            Self::Kernel(api::Status::TooManyInvites) => {
                "Живых приглашений уже четыре — больше нельзя. Дождитесь, пока кто-то воспользуется кодом.".into()
            }
            Self::Kernel(api::Status::FileNotFound) => {
                "Файл не найден или не читается.".into()
            }
            Self::Kernel(api::Status::TooBig) => {
                "Сообщение слишком длинное — лимит 4096 байт.".into()
            }
            Self::Kernel(api::Status::NoSession) => {
                "Контакт не в сети. Напишите, когда он появится.".into()
            }
            Self::Kernel(api::Status::UnsupportedReaction) => {
                "Такой реакции нет.".into()
            }
            Self::Kernel(api::Status::Busy) => "Уже идёт другой звонок.".into(),
            Self::Kernel(api::Status::NoCall) => "Звонок уже завершён.".into(),
            Self::Kernel(api::Status::Internal) => {
                "Внутренний сбой. Перезапустите приложение.".into()
            }
            Self::Kernel(status) => {
                format!("Ошибка ({}). Попробуйте позже.", status.name())
            }
            Self::BadFrame(_) => "Несовместимая версия. Обновите приложение.".into(),
        }
    }
}

/// Запись GET_HISTORY: что ядро помнит о сообщении чата (CORE-API.md §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMsg {
    pub mine: bool,
    /// Пир подтвердил ACK (bit1 flags). Осмысленно только для своих.
    pub delivered: bool,
    pub msg_id: CoreMsgId,
    pub ts_ms: i64,
    pub text: String,
    /// Разметка тела (API 2.0+; у записей ядра 1.x пусто).
    pub entities: Vec<Entity>,
}

/// Содержательный ответ при OK. Ошибки — через `Err(LinkError)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OkReply {
    /// LIST_CONTACTS: контакты в порядке ядра.
    Contacts(Vec<Contact>),
    /// SEND_MESSAGE: msg_id ядра + queued (true = ушло в outbox).
    Sent { msg_id: CoreMsgId, queued: bool },
    /// ACCEPT_INVITE: cid добавленного контакта.
    Added { cid: Cid },
    /// CREATE_INVITE: односторонний инвайт-код.
    Code(String),
    /// SET_AVATAR принят: avatar_id (content addressing) + размер.
    AvatarSet { avatar_id: [u8; 16], size: u32 },
    /// GET_AVATAR_ID: нули — аватарки у контакта нет.
    AvatarId([u8; 16]),
    /// SEND_FILE принят: file_id = SHA-256[0..16], size/chunks —
    /// раскладка передачи. Доставка подтверждается событием FILE_SENT.
    FileQueued {
        file_id: [u8; 16],
        size: u32,
        chunks: u32,
    },
    /// GET_VERSION: версия API демона, semver.
    Version { major: u16, minor: u16, patch: u16 },
    /// GET_HISTORY: всего записей у ядра + окно новейших (старые → новые).
    History {
        total: u32,
        records: Vec<HistoryMsg>,
    },
    /// SEARCH_MESSAGE/GLOBAL_SEARCH_MESSAGE: всего совпадений + страница
    /// (точные новые → старые, затем нечёткие).
    Search { total: u32, hits: Vec<SearchHit> },
    /// GET_PINNED_CHAT: закреплённый чат (None — закрепа нет).
    PinnedChat { cid: Option<Cid> },
    /// GET_PINNED_MESSAGES: msg_id в порядке закрепления.
    PinnedMessages { ids: Vec<CoreMsgId> },
    /// GET_MESSAGE: запись истории (реакции пропущены до своего спринта).
    Message {
        cid: Cid,
        mine: bool,
        delivered: bool,
        ts_ms: i64,
        text: String,
        entities: Vec<Entity>,
    },
    /// START_CALL: идентификатор звонка — им же помечены события
    /// CALL_STATE этого разговора.
    CallStarted { call_id: [u8; 16] },
    /// У команды нет содержательного ответа (block/unblock).
    Done,
}

/// Ответ на [`Request`]. Ошибки команды — данные, не паника: они едут
/// в Store как статус отправки и показываются пользователю.
pub type Reply = Result<OkReply, LinkError>;

/// Стадия звонка из события CALL_STATE (§4.6/§5): 1 offer, 2 accept,
/// 3 reject, 4 end. Reject ядро шлёт вместе с end — клиенту достаточно
/// end для закрытия, reject — для честной причины.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSignal {
    /// Входящий или исходящий вызов создан.
    Offer,
    /// Звонок принят — разговор начался (звук пошёл).
    Accept,
    /// Пир отклонил вызов.
    Reject,
    /// Звонок завершён (любой стороной, любой стадии).
    End,
}

/// События ядра (рассылка всем клиентам, CORE-API.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// Входящее сообщение.
    MessageReceived {
        cid: Cid,
        name: String,
        msg_id: CoreMsgId,
        ts_ms: i64,
        text: String,
        /// Разметка тела (API 2.0+).
        entities: Vec<Entity>,
    },
    /// ACK: сообщение доставлено на устройство пира.
    MessageDelivered { cid: Cid, msg_id: CoreMsgId },
    /// Presence контакта; сессия = живое соединение.
    Presence {
        cid: Cid,
        online: bool,
        last_seen_ms: i64,
    },
    /// Держатель нашего инвайта подключился.
    ContactJoined { cid: Cid, name: String },
    /// Профиль контакта обновился (имя; avatar_id — с S8).
    ContactProfile {
        cid: Cid,
        name: String,
        avatar_id: CoreMsgId,
    },
    /// Пир заблокировал нас.
    BlockedByPeer { cid: Cid },
    /// Пир переслал визитку третьего лица (CONTACT_CARD, API 2.2+, §5):
    /// `cid` — чат-отправитель, `name` — имя из карточки, `addr` пуст,
    /// когда адрес неизвестен. Визитка в историю не пишется — пришла
    /// при живом клиенте или потерялась. Добавить человека из визитки
    /// нельзя: нужно его собственное приглашение.
    ContactCard {
        cid: Cid,
        name: String,
        bundle: [u8; 64],
        addr: String,
    },
    /// Аватарка контакта (AVATAR_RECEIVED, §4.4): ядро само запросило
    /// байты у пира; кэшируем по avatar_id. cid 0 — наша (эхо PROFILE).
    AvatarReceived {
        cid: Cid,
        avatar_id: [u8; 16],
        data: Vec<u8>,
    },
    /// Входящий файл (FILE_RECEIVED, §4.5): ядро уже проверило хэш и
    /// сохранило в ~/.stille-messenger/files/<hex file_id> (0600).
    FileReceived {
        cid: Cid,
        file_id: [u8; 16],
        size: u32,
        name: String,
    },
    /// Все чанки моего файла подтверждены (FILE_SENT): честный статус
    /// «отправлен» для пузыря файла. До события — «отправляется».
    FileSent { cid: Cid, file_id: [u8; 16] },
    /// Реакция собеседника на сообщение (REACTION, §5). add=false —
    /// снятие. Своя реакция событием не приходит — применяется по
    /// ответу SET_REACTION.
    Reaction {
        cid: Cid,
        msg_id: crate::model::msg::CoreMsgId,
        add: bool,
        emoji: char,
        ts_ms: i64,
    },
    /// Смена стадии звонка (CALL_STATE, §4.6/§5): `cid` — собеседник,
    /// `call_id` — какой звонок (их может смениться несколько за сессию,
    /// одна линия на демон).
    CallState {
        cid: Cid,
        call_id: [u8; 16],
        state: CallSignal,
    },
    /// Соединение с демоном потеряно (транспорт; заглушка не присылает).
    Down,
    /// Соединение восстановлено — обновить контакты (ресинк).
    Resync,
}

/// Равноправная реализация транспорта ядра: заглушка сейчас, UDS рядом.
///
/// [`CoreLink::events`] вызывается один раз (подписчик — iced),
/// [`CoreLink::execute`] — из асинхронного контекста iced (tokio).
/// `Hash` — идентичность подписки iced (`Subscription::run_with`).
pub trait CoreLink: Send + Sync + Hash + 'static {
    /// Поток событий ядра; забирает ручку линка (подписчик один).
    /// Требует tokio-контекста.
    fn events(self: Arc<Self>) -> impl Stream<Item = LinkEvent> + Send + 'static;

    /// Исполнить запрос и вернуть ответ. Будущее не заимствует линк:
    /// его можно уложить в Task и дождаться в update.
    fn execute(&self, request: Request) -> impl Future<Output = Reply> + Send + 'static;

    /// Заглушка это или реальное ядро — для подписей в UI.
    fn is_stub(&self) -> bool {
        false
    }
}

/// Мост tokio mpsc → futures Stream (без tokio-stream: poll_recv есть
/// публично). Общий для mock и uds.
pub(crate) struct RecvStream<T>(tokio::sync::mpsc::Receiver<T>);

impl<T> Stream for RecvStream<T> {
    type Item = T;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        // tokio::sync::mpsc::Receiver::poll_recv принимает &mut self.
        self.get_mut().0.poll_recv(cx)
    }
}

#[cfg(test)]
pub(crate) async fn take_events(
    mut stream: impl Stream<Item = LinkEvent> + Unpin,
    n: usize,
) -> std::collections::VecDeque<LinkEvent> {
    use futures_util::StreamExt;
    use std::collections::VecDeque;
    let mut out = VecDeque::with_capacity(n);
    for _ in 0..n {
        if let Some(event) = stream.next().await {
            out.push_back(event);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Совет в тексте ошибки обязан быть выполнимым и честным
    /// (TEXT_STYLE §3): реестр приглашений живёт в памяти демона,
    /// перезапуск приложения его не очистит — остаётся ждать.
    #[test]
    fn too_many_invites_advises_waiting_honestly() {
        let err = LinkError::Kernel(api::Status::TooManyInvites);
        let text = err.user_text();
        assert!(
            text.contains("Дождитесь"),
            "реестр чистится только использованием кодов: {text}"
        );
        assert!(
            !text.contains("Перезапустите"),
            "перезапуск не поможет: {text}"
        );
        assert!(!text.contains("Отзовите"), "отзыва в API нет: {text}");
        assert!(
            !text.contains("демон") && !text.contains("serve"),
            "без техничности: {text}"
        );
    }

    /// Ошибки канала — без технических слов («ядро», «демон», «сессия»);
    /// сбой соединения просит перезапуск приложения.
    #[test]
    fn user_texts_avoid_kernel_jargon() {
        let texts = [
            LinkError::Disconnected.user_text(),
            LinkError::Kernel(api::Status::Internal).user_text(),
            LinkError::Kernel(api::Status::NoSession).user_text(),
            LinkError::Kernel(api::Status::Busy).user_text(),
            LinkError::Kernel(api::Status::NoCall).user_text(),
            LinkError::BadFrame("тест").user_text(),
        ];
        for text in &texts {
            for jargon in ["ядр", "демон", "serve", "сесси"] {
                assert!(!text.contains(jargon), "«{jargon}» в «{text}»");
            }
        }
        assert!(
            texts[0].contains("Перезапустите"),
            "обрыв соединения — совет перезапуска: {}",
            texts[0]
        );
    }

    /// Правила ядра к entities (§4.3) проверяются до отправки — честный
    /// BAD_ARGS от заглушки совпадает с ядерным.
    #[test]
    fn entities_valid_enforces_kernel_rules() {
        let body = "привет, мир"; // 20 байт UTF-8
        assert!(entities_valid(body, &[]).is_ok(), "без разметки — ок");
        // Пересечения и равные offset разрешены (жирный + курсив).
        assert!(
            entities_valid(
                body,
                &[
                    Entity::new(EntityKind::Bold, 0, 6),
                    Entity::new(EntityKind::Italic, 0, 6),
                    Entity::link(7, 4, "https://x".into()),
                ],
            )
            .is_ok(),
            "пересечения легальны"
        );
        assert!(entities_valid(body, &[Entity::new(EntityKind::Bold, 0, 0)]).is_err());
        assert!(entities_valid(body, &[Entity::new(EntityKind::Bold, 15, 8)]).is_err());
        // Не отсортированы по offset.
        assert!(entities_valid(
            body,
            &[
                Entity::new(EntityKind::Bold, 7, 2),
                Entity::new(EntityKind::Italic, 0, 2),
            ],
        )
        .is_err());
        // Ссылка без URL / с длинным URL.
        assert!(entities_valid(body, &[Entity::link(0, 2, String::new())]).is_err());
        assert!(entities_valid(body, &[Entity::link(0, 2, "x".repeat(256))]).is_err());
        // value у простого вида.
        assert!(entities_valid(
            body,
            &[Entity {
                kind: EntityKind::Bold,
                offset: 0,
                len: 2,
                value: "лишнее".into(),
            }]
        )
        .is_err());
        // Больше 32 записей.
        let many: Vec<Entity> = (0..33)
            .map(|i| Entity::new(EntityKind::Bold, 0, 1 + (i == 0) as u16))
            .collect();
        assert!(entities_valid(body, &many).is_err());
        // Язык блока кода — опционален.
        assert!(entities_valid(
            body,
            &[Entity {
                kind: EntityKind::CodeBlock,
                offset: 0,
                len: 6,
                value: "rust".into(),
            }]
        )
        .is_ok());
    }
}
