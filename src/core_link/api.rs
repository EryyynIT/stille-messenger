//! Кодек бинарного протокола ядра — контракт CORE-API
//! (`core/core/src/stille-messenger/app/api.zig`, спецификация `core/core/docs/CORE-API.md`).
//! Подмодуль `core/` на 83ea91e (API 2.2.0); клиент работает и с ядрами 1.x —
//! версия выясняется зондом GET_VERSION (см. [`super::uds`]).
//!
//! Кадр: 16-байтный заголовок (магия 0x5A50, версия, kind, метод/событие,
//! request_id, статус, длина payload) + payload ≤ 16 МиБ. Всё little-endian,
//! никаких JSON и varint. Строки на проводе — str16: `[len u16][байты]`.
//!
//! Здесь только провод: заголовок, коды методов/статусов/событий и примитивы
//! чтения/записи. Раскладки payload методов и событий — в [`super::uds`].

/// Магия кадра: u16 0x5A50 («ZP»), little-endian — на проводе байты `PZ`.
pub const MAGIC: u16 = 0x5A50;
/// Версия протокола на проводе.
pub const VERSION: u8 = 1;
/// Максимум payload одного кадра (16 МиБ, контракт §2); больше ядро
/// не примет, а при приёме закроет соединение (ресинхронизация потока
/// невозможна).
pub const MAX_PAYLOAD: u32 = 16 << 20;
/// Длина заголовка любого кадра.
pub const HEADER_LEN: usize = 16;

/// Тип кадра.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Request = 1,
    Response = 2,
    Event = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Request),
            2 => Some(Self::Response),
            3 => Some(Self::Event),
            _ => None,
        }
    }
}

/// Методы API (GET_VERSION, SEND_MESSAGE, … — имена контракта).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Method {
    Close = 1,
    GetVersion = 2,
    GetIdentity = 3,
    CreateInvite = 4,
    AcceptInvite = 5,
    ListContacts = 6,
    SendMessage = 7,
    SetReaction = 8,
    RemoveReaction = 9,
    GetHistory = 10,
    GetMessage = 11,
    SetName = 12,
    SetAvatar = 13,
    GetAvatarId = 14,
    SendFile = 15,
    StartCall = 16,
    AnswerCall = 17,
    EndCall = 18,
    BlockContact = 19,
    UnblockContact = 20,
    Connect = 21,
    Listen = 22,
    /// API 2.1+: поиск по истории чата / по всем чатам (§4.3).
    SearchMessage = 23,
    GlobalSearchMessage = 24,
    /// API 2.2+: закреп чата (один на демон) (§4.8).
    PinChat = 25,
    GetPinnedChat = 26,
    UnpinChat = 27,
    /// API 2.2+: удалить чат и контакт безвозвратно (§4.8).
    DeleteChat = 28,
    /// API 2.2+: переслать визитку (§4.8); получатель видит CONTACT_CARD.
    SendContact = 29,
    /// API 2.2+: закрепы сообщений (≤16 на чат) (§4.8).
    PinMessage = 30,
    UnpinMessage = 31,
    GetPinnedMessages = 32,
}

impl Method {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Close,
            2 => Self::GetVersion,
            3 => Self::GetIdentity,
            4 => Self::CreateInvite,
            5 => Self::AcceptInvite,
            6 => Self::ListContacts,
            7 => Self::SendMessage,
            8 => Self::SetReaction,
            9 => Self::RemoveReaction,
            10 => Self::GetHistory,
            11 => Self::GetMessage,
            12 => Self::SetName,
            13 => Self::SetAvatar,
            14 => Self::GetAvatarId,
            15 => Self::SendFile,
            16 => Self::StartCall,
            17 => Self::AnswerCall,
            18 => Self::EndCall,
            19 => Self::BlockContact,
            20 => Self::UnblockContact,
            21 => Self::Connect,
            22 => Self::Listen,
            23 => Self::SearchMessage,
            24 => Self::GlobalSearchMessage,
            25 => Self::PinChat,
            26 => Self::GetPinnedChat,
            27 => Self::UnpinChat,
            28 => Self::DeleteChat,
            29 => Self::SendContact,
            30 => Self::PinMessage,
            31 => Self::UnpinMessage,
            32 => Self::GetPinnedMessages,
            _ => return None,
        })
    }
}

/// Коды статусов ответа (OK, NO_CONTACT, … — имена контракта).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    BadFrame,
    UnknownMethod,
    BadArgs,
    NoContact,
    ContactBlocked,
    NotFound,
    TooBig,
    BadInviteCode,
    TooManyInvites,
    FileNotFound,
    UnsupportedReaction,
    NoSession,
    Busy,
    NoCall,
    NoRelay,
    TransportFailed,
    Internal,
    /// Код из будущего ядра; контракт — не паниковать.
    Unknown(u16),
}

impl Status {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::BadFrame,
            2 => Self::UnknownMethod,
            3 => Self::BadArgs,
            4 => Self::NoContact,
            5 => Self::ContactBlocked,
            6 => Self::NotFound,
            7 => Self::TooBig,
            8 => Self::BadInviteCode,
            9 => Self::TooManyInvites,
            10 => Self::FileNotFound,
            11 => Self::UnsupportedReaction,
            12 => Self::NoSession,
            13 => Self::Busy,
            14 => Self::NoCall,
            15 => Self::NoRelay,
            16 => Self::TransportFailed,
            255 => Self::Internal,
            other => Self::Unknown(other),
        }
    }

    /// Числовой код для логов и неузнанных значений.
    pub fn code(self) -> u16 {
        match self {
            Self::Ok => 0,
            Self::BadFrame => 1,
            Self::UnknownMethod => 2,
            Self::BadArgs => 3,
            Self::NoContact => 4,
            Self::ContactBlocked => 5,
            Self::NotFound => 6,
            Self::TooBig => 7,
            Self::BadInviteCode => 8,
            Self::TooManyInvites => 9,
            Self::FileNotFound => 10,
            Self::UnsupportedReaction => 11,
            Self::NoSession => 12,
            Self::Busy => 13,
            Self::NoCall => 14,
            Self::NoRelay => 15,
            Self::TransportFailed => 16,
            Self::Internal => 255,
            Self::Unknown(v) => v,
        }
    }

    /// Машинное имя для логов (SCREAMING_SNAKE, как в CORE-API.md).
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::BadFrame => "BAD_FRAME",
            Self::UnknownMethod => "UNKNOWN_METHOD",
            Self::BadArgs => "BAD_ARGS",
            Self::NoContact => "NO_CONTACT",
            Self::ContactBlocked => "CONTACT_BLOCKED",
            Self::NotFound => "NOT_FOUND",
            Self::TooBig => "TOO_BIG",
            Self::BadInviteCode => "BAD_INVITE_CODE",
            Self::TooManyInvites => "TOO_MANY_INVITES",
            Self::FileNotFound => "FILE_NOT_FOUND",
            Self::UnsupportedReaction => "UNSUPPORTED_REACTION",
            Self::NoSession => "NO_SESSION",
            Self::Busy => "BUSY",
            Self::NoCall => "NO_CALL",
            Self::NoRelay => "NO_RELAY",
            Self::TransportFailed => "TRANSPORT_FAILED",
            Self::Internal => "INTERNAL",
            Self::Unknown(_) => "UNKNOWN",
        }
    }
}

/// События ядра (MESSAGE_RECEIVED, PRESENCE, … — имена контракта).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    MessageReceived = 1,
    MessageDelivered = 2,
    Presence = 3,
    Reaction = 4,
    CallState = 5,
    AvatarReceived = 6,
    FileReceived = 7,
    ContactJoined = 8,
    ContactProfile = 9,
    BlockedByPeer = 10,
    /// API 2.2+: пир переслал визитку третьего лица (SEND_CONTACT).
    ContactCard = 12,
    /// Все чанки файла подтверждены пиром — честный «отправлен» (S10).
    FileSent = 11,
}

impl Event {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::MessageReceived,
            2 => Self::MessageDelivered,
            3 => Self::Presence,
            4 => Self::Reaction,
            5 => Self::CallState,
            6 => Self::AvatarReceived,
            7 => Self::FileReceived,
            8 => Self::ContactJoined,
            9 => Self::ContactProfile,
            10 => Self::BlockedByPeer,
            11 => Self::FileSent,
            12 => Self::ContactCard,
            _ => return None,
        })
    }
}

/// Заголовок кадра (одинаков для request/response/event).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    /// Метод (request/response) или событие (event).
    pub id: u16,
    /// Корреляция: клиент ставит, ядро повторяет в ответе; у событий 0.
    pub request_id: u32,
    /// Статус (только у response; у остальных Ok).
    pub status: Status,
    pub payload_len: u32,
}

/// Записать заголовок в `out[0..16]`.
pub fn encode_header(out: &mut [u8; HEADER_LEN], h: Header) {
    out[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    out[2] = VERSION;
    out[3] = match h.kind {
        Kind::Request => 1,
        Kind::Response => 2,
        Kind::Event => 3,
    };
    out[4..6].copy_from_slice(&h.id.to_le_bytes());
    out[6..10].copy_from_slice(&h.request_id.to_le_bytes());
    out[10..12].copy_from_slice(&h.status.code().to_le_bytes());
    out[12..16].copy_from_slice(&h.payload_len.to_le_bytes());
}

/// Разобрать заголовок. Ошибка — неверная магия, версия, kind, статус
/// или payload больше максимума (соединение не восстановимо).
pub fn decode_header(buf: &[u8; HEADER_LEN]) -> Result<Header, Bad> {
    if u16::from_le_bytes([buf[0], buf[1]]) != MAGIC {
        return Err(Bad);
    }
    if buf[2] != VERSION {
        return Err(Bad);
    }
    let Some(kind) = Kind::from_u8(buf[3]) else {
        return Err(Bad);
    };
    // Статус разбираем числом: неизвестный код не рвёт соединение,
    // ядро моложе/старше клиента — это данные, не поломка кадра.
    let raw_status = u16::from_le_bytes([buf[10], buf[11]]);
    let payload_len = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    if payload_len > MAX_PAYLOAD {
        return Err(Bad);
    }
    Ok(Header {
        kind,
        id: u16::from_le_bytes([buf[4], buf[5]]),
        request_id: u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]),
        status: Status::from_u16(raw_status),
        payload_len,
    })
}

/// Собрать кадр целиком (заголовок + payload) — одна запись в сокет.
pub fn encode_frame(out: &mut Vec<u8>, h: Header, payload: &[u8]) {
    debug_assert_eq!(h.payload_len as usize, payload.len());
    let start = out.len();
    out.resize(start + HEADER_LEN, 0);
    // Важно: `&mut out[r].try_into()` скомпилировался бы как копия [u8; 16]
    // по значению (TryFrom<&[u8]>) — заголовок ушёл бы во временную.
    let hdr: &mut [u8; HEADER_LEN] = (&mut out[start..start + HEADER_LEN])
        .try_into()
        .expect("16 байт");
    encode_header(hdr, h);
    out.extend_from_slice(payload);
}

/// Ошибка чтения кадра/payload: поток не разобран.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bad;

/// Курсор по payload с проверкой границ; любое чтение за пределом — [`Bad`].
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Остались ли непрочитанные байты (строгий разбор: в конце — false).
    pub fn has_more(&self) -> bool {
        self.pos < self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        if self.pos + n > self.buf.len() {
            return Err(Bad);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn read_u8(&mut self) -> Result<u8, Bad> {
        Ok(self.take(1)?[0])
    }

    pub fn read_u16(&mut self) -> Result<u16, Bad> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn read_u32(&mut self) -> Result<u32, Bad> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u64(&mut self) -> Result<u64, Bad> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().expect("8 байт")))
    }

    pub fn read_i64(&mut self) -> Result<i64, Bad> {
        let b = self.take(8)?;
        Ok(i64::from_le_bytes(b.try_into().expect("8 байт")))
    }

    /// Фиксированный кусок байт (msg_id, bundle, avatar_id).
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        self.take(n)
    }

    /// str16: `[len u16][байты]` — сырые UTF-8 без терминатора.
    pub fn read_str16(&mut self) -> Result<&'a [u8], Bad> {
        let len = self.read_u16()? as usize;
        self.take(len)
    }
}

/// Сборка payload запроса (маленькие структуры — Vec хватает).
pub mod put {
    pub fn u8(out: &mut Vec<u8>, v: u8) {
        out.push(v);
    }

    pub fn u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(out: &mut Vec<u8>, v: i64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// str16: `[len u16][байты]`; длиннее 65535 байт нельзя.
    pub fn str16(out: &mut Vec<u8>, s: &str) -> Result<(), super::Bad> {
        let bytes = s.as_bytes();
        if bytes.len() > u16::MAX as usize {
            return Err(super::Bad);
        }
        super::put::u16(out, bytes.len() as u16);
        out.extend_from_slice(bytes);
        Ok(())
    }

    /// bytes16: `[len u16][байты]` — бинарный аналог str16 (аватарки в
    /// событиях, §4.4).
    pub fn bytes16(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), super::Bad> {
        if bytes.len() > u16::MAX as usize {
            return Err(super::Bad);
        }
        super::put::u16(out, bytes.len() as u16);
        out.extend_from_slice(bytes);
        Ok(())
    }

    /// str32: `[len u32][байты]` — большие бинарные (SET_AVATAR, §4.4).
    pub fn bytes32(out: &mut Vec<u8>, bytes: &[u8]) {
        super::put::u32(out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Вектор из контракта: REQUEST GET_VERSION (id 2), request_id 7,
    /// пустой payload. Магия 0x5A50 little-endian = байты `PZ`.
    #[test]
    fn header_request_matches_wire_vector() {
        let mut buf = [0u8; HEADER_LEN];
        encode_header(
            &mut buf,
            Header {
                kind: Kind::Request,
                id: Method::GetVersion as u16,
                request_id: 7,
                status: Status::Ok,
                payload_len: 0,
            },
        );
        assert_eq!(
            buf,
            [
                0x50, 0x5A, 0x01, 0x01, 0x02, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00
            ]
        );
    }

    #[test]
    fn header_response_roundtrip() {
        let mut buf = [0u8; HEADER_LEN];
        encode_header(
            &mut buf,
            Header {
                kind: Kind::Response,
                id: Method::SendMessage as u16,
                request_id: 0x1122_3344,
                status: Status::ContactBlocked,
                payload_len: 17,
            },
        );
        let h = decode_header(&buf).expect("кадр корректен");
        assert_eq!(h.kind, Kind::Response);
        assert_eq!(h.id, Method::SendMessage as u16);
        assert_eq!(h.request_id, 0x1122_3344);
        assert_eq!(h.status, Status::ContactBlocked);
        assert_eq!(h.payload_len, 17);
    }

    #[test]
    fn header_rejects_bad_magic_version_kind_and_oversize() {
        let mut buf = [0u8; HEADER_LEN];
        encode_header(
            &mut buf,
            Header {
                kind: Kind::Request,
                id: 2,
                request_id: 0,
                status: Status::Ok,
                payload_len: 0,
            },
        );

        let mut bad = buf;
        bad[0] = 0x00;
        assert_eq!(decode_header(&bad), Err(Bad));

        let mut bad = buf;
        bad[2] = 9; // не та версия
        assert_eq!(decode_header(&bad), Err(Bad));

        let mut bad = buf;
        bad[3] = 7; // не тот kind
        assert_eq!(decode_header(&bad), Err(Bad));

        let mut bad = buf;
        bad[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert_eq!(decode_header(&bad), Err(Bad));
    }

    #[test]
    fn frame_encoding_is_contiguous() {
        let mut frame = Vec::new();
        encode_frame(
            &mut frame,
            Header {
                kind: Kind::Request,
                id: Method::SendMessage as u16,
                request_id: 1,
                status: Status::Ok,
                payload_len: 8,
            },
            &[0xAA; 8],
        );
        assert_eq!(frame.len(), HEADER_LEN + 8);
        assert_eq!(&frame[HEADER_LEN..], &[0xAA; 8]);
        assert_eq!(frame[0], 0x50);
    }

    #[test]
    fn reader_str16_and_scalars_roundtrip() {
        let mut payload = Vec::new();
        put::u8(&mut payload, 0xAB);
        put::u32(&mut payload, 0xDEAD_BEEF);
        payload.extend_from_slice(&1_700_000_000_123_u64.to_le_bytes());
        payload.extend_from_slice(&(-5_i64).to_le_bytes());
        put::str16(&mut payload, "Аня говорит: привет").expect("str16");

        let mut r = Reader::new(&payload);
        assert_eq!(r.read_u8(), Ok(0xAB));
        assert_eq!(r.read_u32(), Ok(0xDEAD_BEEF));
        assert_eq!(r.read_u64(), Ok(1_700_000_000_123));
        assert_eq!(r.read_i64(), Ok(-5));
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok("Аня говорит: привет")
        );
        assert!(!r.has_more(), "строгий разбор: payload прочитан до конца");
    }

    #[test]
    fn reader_truncated_input_is_bad() {
        let mut payload = Vec::new();
        put::str16(&mut payload, "12345").expect("str16");
        let mut r = Reader::new(&payload[..payload.len() - 1]);
        assert_eq!(r.read_str16(), Err(Bad));
    }

    #[test]
    fn method_status_event_codes_are_pinned() {
        // Контракт CORE-API.md §3–5: числовые значения стабильны.
        assert_eq!(Method::Close as u16, 1);
        assert_eq!(Method::ListContacts as u16, 6);
        assert_eq!(Method::SendMessage as u16, 7);
        assert_eq!(Method::AcceptInvite as u16, 5);
        assert_eq!(Method::BlockContact as u16, 19);
        assert_eq!(Status::NoContact.code(), 4);
        assert_eq!(Status::TooBig.code(), 7);
        assert_eq!(Status::BadInviteCode.code(), 8);
        assert_eq!(Status::NoSession.code(), 12);
        assert_eq!(Event::MessageReceived as u16, 1);
        assert_eq!(Event::MessageDelivered as u16, 2);
        assert_eq!(Event::Presence as u16, 3);
        assert_eq!(Event::ContactJoined as u16, 8);

        assert_eq!(Method::from_u16(7), Some(Method::SendMessage));
        assert_eq!(Method::from_u16(0), None);
        assert_eq!(Method::from_u16(23), Some(Method::SearchMessage));
        assert_eq!(Method::from_u16(24), Some(Method::GlobalSearchMessage));
        assert_eq!(Method::from_u16(25), Some(Method::PinChat));
        assert_eq!(Method::from_u16(32), Some(Method::GetPinnedMessages));
        assert_eq!(Method::from_u16(28), Some(Method::DeleteChat));
        assert_eq!(Method::from_u16(29), Some(Method::SendContact));
        assert_eq!(Method::from_u16(33), None);
        assert_eq!(Status::from_u16(4), Status::NoContact);
        assert_eq!(Status::from_u16(600), Status::Unknown(600));
        assert_eq!(Event::from_u16(10), Some(Event::BlockedByPeer));
        assert_eq!(Event::from_u16(11), Some(Event::FileSent));
        assert_eq!(Event::from_u16(12), Some(Event::ContactCard));
    }

    #[test]
    fn unknown_status_does_not_break_frame_parsing() {
        let mut buf = [0u8; HEADER_LEN];
        encode_header(
            &mut buf,
            Header {
                kind: Kind::Response,
                id: 6,
                request_id: 3,
                status: Status::from_u16(777),
                payload_len: 0,
            },
        );
        let h = decode_header(&buf).expect("кадр корректен");
        assert_eq!(h.status, Status::Unknown(777));
    }
}
