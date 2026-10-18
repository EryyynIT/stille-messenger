//! Сообщение ленты: направление, время, статус отправки.

use std::fmt;

use crate::core_link::Entity;
use crate::model::cid::Cid;

/// Локальный номер строки ленты — ключ UI (привязка ответа к пузырю).
/// Не путать с [`CoreMsgId`]: настоящий msg_id ядра живёт в `Msg::core_id`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MsgId(pub u64);

/// msg_id ядра: 16 случайных байт, идентифицируют сообщение глобально
/// (CORE-API.md §1). У моих сообщений появляется после ответа SEND_MESSAGE,
/// у чужих — приходит в событии MESSAGE_RECEIVED.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CoreMsgId(pub [u8; 16]);

impl fmt::Display for CoreMsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Автор строки ленты.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Моё сообщение.
    Out,
    /// Сообщение собеседника.
    In(Cid),
    /// Служебная строка ядра (sys).
    Sys,
    /// Визитка третьего лица (CONTACT_CARD, S13): не сообщение — у ядра
    /// нет ни msg_id, ни записи в истории, поэтому не группируется,
    /// не закрепляется и не ищется.
    Card,
}

/// Файл-вложение (S10): имя/размер с провода, file_id = SHA-256[0..16],
/// path — где лежит для открытия (входящий: ~/.stille-messenger/files/<hex>;
/// свой: исходный путь). Статус передачи — поле send сообщения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub name: String,
    pub size: u32,
    pub file_id: [u8; 16],
    pub path: Option<String>,
}

/// Реакция на сообщение (S15): эмодзи, счётчик, моя ли (моя — кликом
/// снимается). Ядро историю реакций не отдаёт — состояние живёт, пока
/// жива лента в памяти.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: char,
    pub count: u32,
    pub mine: bool,
}

/// Честный статус отправки — жизненный цикл моего сообщения
/// по модели доставки ядра (CORE-API.md §6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SendState {
    Sending,
    Sent,
    /// Ядро положило сообщение в outbox: уйдёт, когда контакт появится.
    Queued,
    /// Пир подтвердил ACK — сообщение на его устройстве.
    Delivered,
    /// Не ушло + причина для пользователя.
    Failed(String),
}

/// Строка ленты.
#[derive(Clone, Debug)]
pub struct Msg {
    pub id: MsgId,
    pub dir: Dir,
    pub ts_ms: i64,
    pub body: String,
    /// Разметка тела (S14, API 2.0+): байтовые offset/len в body.
    pub entities: Vec<Entity>,
    /// Реакции (S15): событийные, в истории ядра их нет — после
    /// перезапуска пусто (честный предел контракта).
    pub reactions: Vec<Reaction>,
    /// Файл (S10): сообщение-вложение; body дублирует имя для поиска.
    pub file: Option<FileRef>,
    /// Ответ на сообщение (будущее): msg_id исходного. Контракта ядра
    /// (SEND_REPLY_MESSAGE) пока нет — поле модели подготовлено, UI и
    /// провод не подключены; всегда None.
    pub reply_to: Option<CoreMsgId>,
    pub send: SendState,
    /// msg_id ядра, если известен; по нему приходит MESSAGE_DELIVERED.
    pub core_id: Option<CoreMsgId>,
}

impl Msg {
    pub fn incoming(
        id: MsgId,
        cid: Cid,
        ts_ms: i64,
        body: String,
        core_id: Option<CoreMsgId>,
        entities: Vec<Entity>,
    ) -> Self {
        Self {
            id,
            dir: Dir::In(cid),
            ts_ms,
            body,
            entities,
            reactions: Vec::new(),
            file: None,
            reply_to: None,
            send: SendState::Sent,
            core_id,
        }
    }

    pub fn outgoing(id: MsgId, ts_ms: i64, body: String, entities: Vec<Entity>) -> Self {
        Self {
            id,
            dir: Dir::Out,
            ts_ms,
            body,
            entities,
            reactions: Vec::new(),
            file: None,
            reply_to: None,
            send: SendState::Sending,
            core_id: None,
        }
    }

    pub fn sys(ts_ms: i64, body: String) -> Self {
        Self {
            id: MsgId(0),
            dir: Dir::Sys,
            ts_ms,
            body,
            entities: Vec::new(),
            reactions: Vec::new(),
            file: None,
            reply_to: None,
            send: SendState::Sent,
            core_id: None,
        }
    }

    /// Визитка в ленте (S13): body — имя из карточки.
    pub fn card(ts_ms: i64, name: String) -> Self {
        Self {
            id: MsgId(0),
            dir: Dir::Card,
            ts_ms,
            body: name,
            entities: Vec::new(),
            reactions: Vec::new(),
            file: None,
            reply_to: None,
            send: SendState::Sent,
            core_id: None,
        }
    }

    /// Исходящий файл (S10): Sending до события FILE_SENT.
    pub fn outgoing_file(id: MsgId, ts_ms: i64, file: FileRef) -> Self {
        Self {
            id,
            dir: Dir::Out,
            ts_ms,
            body: file.name.clone(),
            entities: Vec::new(),
            reactions: Vec::new(),
            file: Some(file),
            reply_to: None,
            send: SendState::Sending,
            core_id: None,
        }
    }

    /// Входящий файл (S10): ядро уже сохранил его на диск.
    pub fn incoming_file(id: MsgId, cid: Cid, ts_ms: i64, file: FileRef) -> Self {
        Self {
            id,
            dir: Dir::In(cid),
            ts_ms,
            body: file.name.clone(),
            entities: Vec::new(),
            reactions: Vec::new(),
            file: Some(file),
            reply_to: None,
            send: SendState::Sent,
            core_id: None,
        }
    }

    /// Автор группы: мои и sys всегда свои группы; чужие — по cid.
    /// Визитки никогда не группируются (feed ставит их отдельно).
    pub fn is_same_group_as(&self, other: &Msg) -> bool {
        match (&self.dir, &other.dir) {
            (Dir::In(a), Dir::In(b)) => a == b,
            (Dir::Out, Dir::Out) => true,
            (Dir::Sys, Dir::Sys) => true,
            _ => false,
        }
    }
}
