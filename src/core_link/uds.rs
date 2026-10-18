//! UDS-транспорт бинарного API: соединение с демоном `stille-messenger-core serve`
//! (`$HOME/.stille-messenger/api.sock`), корреляция ответов по request_id, события
//! в поток, backoff-ресинк при обрыве. Здесь же раскладки payload методов
//! и событий — структурный слой поверх кодека [`super::api`].
//!
//! Версия API выясняется зондирующим GET_VERSION (readiness probe
//! из CORE-API.md) перед первым запросом соединения: API 2.x требует
//! entity tail в SEND_MESSAGE и в записях GET_HISTORY, API 1.x лишних
//! байтов не шлёт и не принимает.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::api::{self, Header, Kind, Method, Status};
use super::{
    CallSignal, CoreLink, Entity, EntityKind, HistoryMsg, LinkError, LinkEvent, OkReply,
    RecvStream, Reply, Request, SearchHit,
};
use crate::model::cid::Cid;
use crate::model::msg::CoreMsgId;
use crate::model::store::Contact;

/// Сколько ждать ответа ядра, прежде чем считать запрос зависшим.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// Потолок backoff-ресинка (0.5 → 1 → … → 30 с).
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Ёмкость очереди исходящих кадров и событий.
const CHANNEL_CAP: usize = 256;

/// Ждущие ответа: request_id → канал ответа. Arc — читатель живёт
/// в отдельной задаче и не заимствует Inner.
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<RawReply, LinkError>>>>>;

/// Сырой ответ ядра: статус + payload; раскладку знает только decode_reply.
struct RawReply {
    status: Status,
    payload: Vec<u8>,
}

/// Реальный канал с ядром: демон `stille-messenger-core serve` обязан быть запущен
/// (при отсутствии GUI сам попытается его поднять — один раз).
pub struct UdsLink {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    /// База backoff; тесты ставят маленькую.
    backoff: Duration,
    /// Пробовать спавнить `stille-messenger-core serve`, если сокета нет.
    spawn_daemon: bool,
    /// Живое соединение: очередь кадров записи.
    conn: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pending: Pending,
    next_id: AtomicU32,
    /// Major-версия API демона; 0 — ещё не выяснена (сбрасывается при
    /// каждом новом соединении — между ними демон могли заменить).
    api_major: AtomicU16,
    /// Minor-версия API демона; осмысленна вместе с `api_major`: гейт
    /// фич вроде поиска (2.1+) смотрит пару (major, minor).
    api_minor: AtomicU16,
    events_tx: mpsc::Sender<LinkEvent>,
    events_rx: Mutex<Option<mpsc::Receiver<LinkEvent>>>,
}

impl UdsLink {
    /// Канал к демону текущего пользователя ($HOME/.stille-messenger/api.sock).
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = Path::new(&home).join(".stille-messenger").join("api.sock");
        Self::with_config(path, Duration::from_millis(500), true)
    }

    /// Точки входа для тестов: путь, база backoff, спавн демона.
    #[doc(hidden)]
    pub fn with_config(path: PathBuf, backoff: Duration, spawn_daemon: bool) -> Self {
        let (events_tx, events_rx) = mpsc::channel(CHANNEL_CAP);
        Self {
            inner: Arc::new(Inner {
                path,
                backoff,
                spawn_daemon,
                conn: Mutex::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
                next_id: AtomicU32::new(1),
                api_major: AtomicU16::new(0),
                api_minor: AtomicU16::new(0),
                events_tx,
                events_rx: Mutex::new(Some(events_rx)),
            }),
        }
    }
}

impl Default for UdsLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Hash for UdsLink {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        Arc::as_ptr(&self.inner).hash(hasher);
    }
}

impl CoreLink for UdsLink {
    fn events(self: Arc<Self>) -> impl Stream<Item = LinkEvent> + Send + 'static {
        let Some(rx) = self.inner.events_rx.lock().expect("rx не занят").take() else {
            panic!("подписка на события канала уже взята");
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(inner.run());
        RecvStream(rx)
    }

    fn execute(&self, request: Request) -> impl Future<Output = Reply> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move { inner.request(request).await }
    }
}

impl Inner {
    /// Супервизор соединения: коннект → обслуживание → обрыв → backoff.
    /// Каждое успешное соединение начинается с Resync: UI перечитает
    /// контакты, лента в памяти GUI остаётся.
    async fn run(self: Arc<Self>) {
        let mut backoff = self.backoff;
        let mut daemon_spawned = false;
        loop {
            match UnixStream::connect(&self.path).await {
                Ok(stream) => {
                    backoff = self.backoff;
                    self.api_major.store(0, Ordering::Relaxed);
                    self.api_minor.store(0, Ordering::Relaxed);
                    let _ = self.events_tx.send(LinkEvent::Resync).await;
                    self.serve(stream).await;
                    let _ = self.events_tx.send(LinkEvent::Down).await;
                }
                Err(_) => {
                    if self.spawn_daemon && !daemon_spawned {
                        daemon_spawned = true;
                        Self::spawn_daemon().await;
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
        }
    }

    /// Запускает `stille-messenger-core serve` из PATH; для свежего $HOME сперва `stille-messenger-core init`
    /// (демон без сида сразу выходит). GUI не становится его сидом:
    /// процесс ждут в отдельной задаче только ради reap.
    async fn spawn_daemon() {
        let home = std::env::var("HOME").unwrap_or_default();
        let seed = Path::new(&home).join(".stille-messenger/zig-seed");
        let daemon = "stille-messenger-core";
        if !seed.exists() {
            let _ = tokio::process::Command::new(daemon)
                .arg("init")
                .output()
                .await;
        }
        let Ok(mut child) = tokio::process::Command::new(daemon).arg("serve").spawn() else {
            return; // бинарника нет — статус «нет связи», не шумим
        };
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }

    /// Обслуживание одного соединения до EOF: читатель собирает кадры,
    /// писатель сериализует записи; общий стейт — только pending.
    async fn serve(&self, stream: UnixStream) {
        let (mut read_half, mut write_half) = stream.into_split();
        let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAP);
        *self.conn.lock().expect("conn не отравлен") = Some(writer_tx.clone());

        let pending = Arc::clone(&self.pending);
        let events_tx = self.events_tx.clone();
        let reader = tokio::spawn(async move {
            let mut buf = [0u8; api::HEADER_LEN];
            loop {
                if read_half.read_exact(&mut buf).await.is_err() {
                    break;
                }
                let Ok(header) = api::decode_header(&buf) else {
                    break; // магия/версия/длина — поток не восстановим
                };
                let mut payload = vec![0u8; header.payload_len as usize];
                if header.payload_len > 0 && read_half.read_exact(&mut payload).await.is_err() {
                    break;
                }
                match header.kind {
                    Kind::Response => resolve_pending(&pending, header, payload),
                    Kind::Event => {
                        if let Some(event) = decode_event(&header, &payload) {
                            let _ = events_tx.send(event).await;
                        }
                        // Неизвестное событие моложе клиента — пропускаем.
                    }
                    Kind::Request => break, // ядро не спрашивает клиента
                }
            }
        });

        let writer = tokio::spawn(async move {
            while let Some(frame) = writer_rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let _ = reader.await;
        writer.abort();
        *self.conn.lock().expect("conn не отравлен") = None;
        // Все зависшие запросы получают Disconnected.
        self.pending
            .lock()
            .expect("pending не отравлен")
            .drain()
            .for_each(|(_, tx)| {
                let _ = tx.send(Err(LinkError::Disconnected));
            });
    }

    async fn request(&self, request: Request) -> Reply {
        // Версия API — первый запрос соединения: readiness probe из
        // CORE-API.md. От major зависят раскладки tail-зависимых методов;
        // сбой зонда не роняет запрос — ядро без GET_VERSION не встречалось.
        if self.api_major.load(Ordering::Relaxed) == 0 {
            let (method, payload) = encode_request(&Request::Version, false);
            let _ = self.roundtrip(method, payload, false).await;
        }
        let api2 = self.api_major.load(Ordering::Relaxed) >= 2;
        let (method, payload) = encode_request(&request, api2);
        let mut reply = self.roundtrip(method, payload, api2).await;
        // Чатовый поиск не несёт cid в записях ответа — восстанавливаем
        // из запроса, чтобы SearchHit всегда знал свой чат.
        if let (Request::Search { cid: Some(cid), .. }, Ok(OkReply::Search { hits, .. })) =
            (&request, &mut reply)
        {
            for hit in hits.iter_mut() {
                hit.cid = *cid;
            }
        }
        reply
    }

    /// Один запрос-ответ: request_id, кадр, ожидание с таймаутом.
    /// `api2` снимок версии на момент кодирования — раскладка ответа
    /// (entity tail в записях истории) должна совпадать с запросом.
    async fn roundtrip(&self, method: Method, payload: Vec<u8>, api2: bool) -> Reply {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Лок берём только чтобы клонировать очередь записи: никакие
        // локи не держатся через await.
        let writer = {
            let conn = self.conn.lock().expect("conn не отравлен");
            match conn.as_ref() {
                Some(writer) => writer.clone(),
                None => return Err(LinkError::Disconnected),
            }
        };

        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending не отравлен")
            .insert(request_id, tx);

        let mut frame = Vec::with_capacity(api::HEADER_LEN + payload.len());
        api::encode_frame(
            &mut frame,
            Header {
                kind: Kind::Request,
                id: method as u16,
                request_id,
                status: Status::Ok,
                payload_len: payload.len() as u32,
            },
            &payload,
        );

        if writer.send(frame).await.is_err() {
            self.pending
                .lock()
                .expect("pending не отравлен")
                .remove(&request_id);
            return Err(LinkError::Disconnected);
        }

        match timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(Ok(raw))) => {
                let reply = decode_reply(method, raw, api2);
                if let Ok(OkReply::Version { major, minor, .. }) = &reply {
                    self.api_major.store(*major, Ordering::Relaxed);
                    self.api_minor.store(*minor, Ordering::Relaxed);
                }
                reply
            }
            // Дренаж при обрыве: всем отправлен Disconnected.
            Ok(Ok(Err(err))) => Err(err),
            // Канал ответа закрыт без отправки — соединение умерло.
            Ok(Err(_dropped)) => Err(LinkError::Disconnected),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending не отравлен")
                    .remove(&request_id);
                Err(LinkError::Disconnected)
            }
        }
    }
}

/// Ответ ядра на дождавшийся запрос.
fn resolve_pending(pending: &Pending, header: Header, payload: Vec<u8>) {
    if let Some(tx) = pending
        .lock()
        .expect("pending не отравлен")
        .remove(&header.request_id)
    {
        let _ = tx.send(Ok(RawReply {
            status: header.status,
            payload,
        }));
    }
    // Ответ без ждущего: таймаут уже истёк — молча выбрасываем.
}

/// Запрос → метод API + payload (раскладки — CORE-API.md §4).
/// `api2` — демон выяснен как API 2.x: SEND_MESSAGE дополняется
/// entity tail (пустой список сущностей).
fn encode_request(request: &Request, api2: bool) -> (Method, Vec<u8>) {
    let mut payload = Vec::new();
    let method = match request {
        Request::Contacts => Method::ListContacts,
        Request::Version => Method::GetVersion,
        Request::SendMsg {
            cid,
            text,
            entities,
        } => {
            api::put::u32(&mut payload, cid.0 as u32);
            api::put::str16(&mut payload, text).expect("текст уже проверен на 4096");
            if api2 {
                // Entity tail (API 2.x, §4.3). API 1.x лишний байт
                // отвергает, поэтому хвост ставится только после BadFrame
                // от демона; на 1.x разметки нет (ui гейтит ввод).
                debug_assert!(
                    entities.iter().all(|e| e.value.len() <= 255),
                    "value_len u8"
                );
                put_entity_tail(&mut payload, entities);
            }
            Method::SendMessage
        }
        // my_name пустой — ядро подставит «друг».
        Request::Add { code } => {
            api::put::str16(&mut payload, code).expect("код короче 65535");
            api::put::str16(&mut payload, "").expect("пустая строка");
            Method::AcceptInvite
        }
        // name пустой — ядро подставит «stille»; addr пустой — LAN-анонс;
        // relay=1 требует addr: соединение идёт через релей, peer IP не видит.
        Request::Invite { relay, addr } => {
            api::put::u8(&mut payload, u8::from(*relay));
            api::put::str16(&mut payload, "").expect("пустая строка");
            api::put::str16(&mut payload, addr).expect("адрес короче 65535");
            Method::CreateInvite
        }
        // SET_AVATAR (§4.4): [data str32] — байты картинки как есть.
        Request::SetAvatar { data } => {
            api::put::bytes32(&mut payload, data);
            Method::SetAvatar
        }
        // GET_AVATAR_ID (§4.4): [cid u32].
        Request::GetAvatarId { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::GetAvatarId
        }
        // SEND_FILE (§4.5): [cid u32][path str16] — путь читает демон.
        Request::SendFile { cid, path } => {
            api::put::u32(&mut payload, cid.0 as u32);
            api::put::str16(&mut payload, path).expect("путь короче 65535");
            Method::SendFile
        }
        // SET/REMOVE_REACTION (§4.4): [cid u32][msg_id 16][codepoint u32].
        Request::SetReaction { cid, msg_id, emoji }
        | Request::RemoveReaction { cid, msg_id, emoji } => {
            api::put::u32(&mut payload, cid.0 as u32);
            payload.extend_from_slice(&msg_id.0);
            api::put::u32(&mut payload, *emoji as u32);
            if matches!(request, Request::SetReaction { .. }) {
                Method::SetReaction
            } else {
                Method::RemoveReaction
            }
        }
        Request::Block { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::BlockContact
        }
        Request::Unblock { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::UnblockContact
        }
        Request::History { cid, limit } => {
            api::put::u32(&mut payload, cid.0 as u32);
            api::put::u32(&mut payload, *limit);
            Method::GetHistory
        }
        // SEARCH_MESSAGE (23) / GLOBAL_SEARCH_MESSAGE (24), API 2.1+.
        // Запрос: [cid u32 (только чатовый)][query str16][limit u32][offset u32].
        // Гейт по версии — на стороне UI: сюда запрос доезжает, только когда
        // демон уже выяснен как 2.1+.
        Request::Search {
            cid,
            query,
            limit,
            offset,
        } => {
            if let Some(cid) = cid {
                api::put::u32(&mut payload, cid.0 as u32);
            }
            api::put::str16(&mut payload, query).expect("запрос короче 65535");
            api::put::u32(&mut payload, *limit);
            api::put::u32(&mut payload, *offset);
            if cid.is_some() {
                Method::SearchMessage
            } else {
                Method::GlobalSearchMessage
            }
        }
        // Закрепы (API 2.2+, §4.8): раскладки тривиальны.
        Request::PinChat { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::PinChat
        }
        Request::UnpinChat => Method::UnpinChat,
        Request::PinnedChat => Method::GetPinnedChat,
        Request::PinnedMessages { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::GetPinnedMessages
        }
        Request::PinMessage { cid, msg_id } => {
            api::put::u32(&mut payload, cid.0 as u32);
            payload.extend_from_slice(&msg_id.0);
            Method::PinMessage
        }
        Request::UnpinMessage { cid, msg_id } => {
            api::put::u32(&mut payload, cid.0 as u32);
            payload.extend_from_slice(&msg_id.0);
            Method::UnpinMessage
        }
        Request::GetMessage { msg_id } => {
            payload.extend_from_slice(&msg_id.0);
            Method::GetMessage
        }
        // Визитки и удаление чата (API 2.2+, §4.8).
        Request::DeleteChat { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::DeleteChat
        }
        Request::SendContact { to, contact } => {
            api::put::u32(&mut payload, to.0 as u32);
            // None — моя карточка, на проводе contact_cid = 0.
            api::put::u32(&mut payload, contact.map_or(0, |c| c.0 as u32));
            Method::SendContact
        }
        // Имя профиля (§4.4): [name str16 1..32].
        Request::SetName { name } => {
            api::put::str16(&mut payload, name).expect("имя короче 65535");
            Method::SetName
        }
        // Звонки (§4.6): START_CALL — [cid u32], ответ [call_id 16];
        // ANSWER/END — пустые.
        Request::StartCall { cid } => {
            api::put::u32(&mut payload, cid.0 as u32);
            Method::StartCall
        }
        Request::AnswerCall => Method::AnswerCall,
        Request::EndCall => Method::EndCall,
    };
    (method, payload)
}

/// Ответ → типизированный Reply по методу; статус != OK — Err(Kernel).
/// `api2` — демон ответил как API 2.x: записи GET_HISTORY обязаны нести
/// entity tail, у API 1.x их нет вовсе.
fn decode_reply(method: Method, raw: RawReply, api2: bool) -> Reply {
    if raw.status != Status::Ok {
        return Err(LinkError::Kernel(raw.status));
    }
    let reply = match method {
        Method::ListContacts => OkReply::Contacts(decode_contacts(&raw.payload)?),
        Method::SendMessage => {
            let mut r = api::Reader::new(&raw.payload);
            let mut msg_id = [0u8; 16];
            msg_id.copy_from_slice(
                r.read_bytes(16)
                    .map_err(|_| LinkError::BadFrame("msg_id"))?,
            );
            let queued = r.read_u8().map_err(|_| LinkError::BadFrame("queued"))? == 1;
            OkReply::Sent {
                msg_id: CoreMsgId(msg_id),
                queued,
            }
        }
        Method::AcceptInvite => {
            let mut r = api::Reader::new(&raw.payload);
            let cid = r.read_u32().map_err(|_| LinkError::BadFrame("cid"))?;
            OkReply::Added {
                cid: Cid(i64::from(cid)),
            }
        }
        Method::CreateInvite => {
            let mut r = api::Reader::new(&raw.payload);
            let code = r.read_str16().map_err(|_| LinkError::BadFrame("code"))?;
            OkReply::Code(String::from_utf8_lossy(code).into_owned())
        }
        Method::GetVersion => {
            let mut r = api::Reader::new(&raw.payload);
            let major = r.read_u16().map_err(|_| LinkError::BadFrame("major"))?;
            let minor = r.read_u16().map_err(|_| LinkError::BadFrame("minor"))?;
            let patch = r.read_u16().map_err(|_| LinkError::BadFrame("patch"))?;
            OkReply::Version {
                major,
                minor,
                patch,
            }
        }
        Method::GetHistory => {
            let mut r = api::Reader::new(&raw.payload);
            let total = r.read_u32().map_err(|_| LinkError::BadFrame("total"))?;
            let count = r.read_u32().map_err(|_| LinkError::BadFrame("count"))?;
            let mut records = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let flags = r.read_u8().map_err(|_| LinkError::BadFrame("flags"))?;
                let mut msg_id = [0u8; 16];
                msg_id.copy_from_slice(
                    r.read_bytes(16)
                        .map_err(|_| LinkError::BadFrame("msg_id"))?,
                );
                let ts_ms = r.read_u64().map_err(|_| LinkError::BadFrame("ts"))? as i64;
                let text = r.read_str16().map_err(|_| LinkError::BadFrame("body"))?;
                // Entity tail есть только у API 2.x; позиционно отличить его
                // от флагов следующей записи нельзя — решает версия зонда.
                let entities = if api2 {
                    read_entity_tail(&mut r)?
                } else {
                    Vec::new()
                };
                records.push(HistoryMsg {
                    mine: flags & 0b01 != 0,
                    delivered: flags & 0b10 != 0,
                    msg_id: CoreMsgId(msg_id),
                    ts_ms,
                    text: String::from_utf8_lossy(text).into_owned(),
                    entities,
                });
            }
            OkReply::History { total, records }
        }
        // Ответ поиска (API 2.1+): [total u32][count u32] × {[cid u32 —
        // только глобальный][flags u8][msg_id 16][ts u64][body str16]
        // [entity tail]}. Метод существует только на 2.1+, поэтому
        // entity tail обязателен и версия зонда не нужна.
        Method::SearchMessage | Method::GlobalSearchMessage => {
            let global = method == Method::GlobalSearchMessage;
            let mut r = api::Reader::new(&raw.payload);
            let total = r.read_u32().map_err(|_| LinkError::BadFrame("total"))?;
            let count = r.read_u32().map_err(|_| LinkError::BadFrame("count"))?;
            let mut hits = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let cid = if global {
                    r.read_u32().map_err(|_| LinkError::BadFrame("cid"))?
                } else {
                    0 // заполняется вызывающим: чатовый поиск знает свой cid
                };
                let flags = r.read_u8().map_err(|_| LinkError::BadFrame("flags"))?;
                let mut msg_id = [0u8; 16];
                msg_id.copy_from_slice(
                    r.read_bytes(16)
                        .map_err(|_| LinkError::BadFrame("msg_id"))?,
                );
                let ts_ms = r.read_u64().map_err(|_| LinkError::BadFrame("ts"))? as i64;
                let body = r.read_str16().map_err(|_| LinkError::BadFrame("body"))?;
                let entities = read_entity_tail(&mut r)?;
                hits.push(SearchHit {
                    cid: Cid(i64::from(cid)),
                    mine: flags & 0b001 != 0,
                    delivered: flags & 0b010 != 0,
                    fuzzy: flags & 0b100 != 0,
                    msg_id: CoreMsgId(msg_id),
                    ts_ms,
                    body: String::from_utf8_lossy(body).into_owned(),
                    entities,
                });
            }
            OkReply::Search { total, hits }
        }
        // START_CALL (§4.6): [call_id 16].
        Method::StartCall => {
            let mut r = api::Reader::new(&raw.payload);
            let mut call_id = [0u8; 16];
            call_id.copy_from_slice(
                r.read_bytes(16)
                    .map_err(|_| LinkError::BadFrame("call_id"))?,
            );
            OkReply::CallStarted { call_id }
        }
        // SET_AVATAR (§4.4): [avatar_id 16][size u32].
        Method::SetAvatar => {
            let mut r = api::Reader::new(&raw.payload);
            let mut avatar_id = [0u8; 16];
            avatar_id.copy_from_slice(
                r.read_bytes(16)
                    .map_err(|_| LinkError::BadFrame("avatar_id"))?,
            );
            let size = r.read_u32().map_err(|_| LinkError::BadFrame("size"))?;
            OkReply::AvatarSet { avatar_id, size }
        }
        // GET_AVATAR_ID (§4.4): [avatar_id 16], нули — аватарки нет.
        Method::GetAvatarId => {
            let mut r = api::Reader::new(&raw.payload);
            let mut avatar_id = [0u8; 16];
            avatar_id.copy_from_slice(
                r.read_bytes(16)
                    .map_err(|_| LinkError::BadFrame("avatar_id"))?,
            );
            OkReply::AvatarId(avatar_id)
        }
        // SEND_FILE (§4.5): [file_id 16][size u32][chunks u32].
        Method::SendFile => {
            let mut r = api::Reader::new(&raw.payload);
            let mut file_id = [0u8; 16];
            file_id.copy_from_slice(
                r.read_bytes(16)
                    .map_err(|_| LinkError::BadFrame("file_id"))?,
            );
            let size = r.read_u32().map_err(|_| LinkError::BadFrame("size"))?;
            let chunks = r.read_u32().map_err(|_| LinkError::BadFrame("chunks"))?;
            OkReply::FileQueued {
                file_id,
                size,
                chunks,
            }
        }
        // GET_PINNED_CHAT (2.2+): [cid u32], 0 = закрепа нет.
        Method::GetPinnedChat => {
            let mut r = api::Reader::new(&raw.payload);
            let cid = r.read_u32().map_err(|_| LinkError::BadFrame("cid"))?;
            OkReply::PinnedChat {
                cid: (cid != 0).then_some(Cid(i64::from(cid))),
            }
        }
        // GET_PINNED_MESSAGES (2.2+): [count u32] × [msg_id 16].
        Method::GetPinnedMessages => {
            let mut r = api::Reader::new(&raw.payload);
            let count = r.read_u32().map_err(|_| LinkError::BadFrame("count"))?;
            let mut ids = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let mut msg_id = [0u8; 16];
                msg_id.copy_from_slice(
                    r.read_bytes(16)
                        .map_err(|_| LinkError::BadFrame("msg_id"))?,
                );
                ids.push(CoreMsgId(msg_id));
            }
            OkReply::PinnedMessages { ids }
        }
        // GET_MESSAGE: [cid u32][flags u8][ts u64][body str16]; у API 2.x
        // далее entity tail и список реакций — пропускаем до своих спринтов.
        Method::GetMessage => {
            let mut r = api::Reader::new(&raw.payload);
            let cid = r.read_u32().map_err(|_| LinkError::BadFrame("cid"))?;
            let flags = r.read_u8().map_err(|_| LinkError::BadFrame("flags"))?;
            let ts_ms = r.read_u64().map_err(|_| LinkError::BadFrame("ts"))? as i64;
            let body = r.read_str16().map_err(|_| LinkError::BadFrame("body"))?;
            let entities = if api2 {
                read_entity_tail(&mut r)?
            } else {
                Vec::new()
            };
            if api2 {
                // Реакции: [react_count u32] × {[flags u8][codepoint u32][ts u64]}.
                let reacts = r.read_u32().map_err(|_| LinkError::BadFrame("reacts"))?;
                for _ in 0..reacts {
                    r.read_u8()
                        .map_err(|_| LinkError::BadFrame("react_flags"))?;
                    r.read_u32()
                        .map_err(|_| LinkError::BadFrame("react_codepoint"))?;
                    r.read_u64().map_err(|_| LinkError::BadFrame("react_ts"))?;
                }
            }
            OkReply::Message {
                cid: Cid(i64::from(cid)),
                mine: flags & 0b01 != 0,
                delivered: flags & 0b10 != 0,
                ts_ms,
                text: String::from_utf8_lossy(body).into_owned(),
                entities,
            }
        }
        _ => OkReply::Done,
    };
    Ok(reply)
}

/// Разобрать entity tail записи (API 2.0+, §4.3):
/// `[ent_count u8] × {[kind u8][offset u16][len u16][value_len u8][value]}`.
/// Неизвестный kind из будущего ядра пропускается (рендерить его всё
/// равно нечем), обрезанная структура — BadFrame. Семантику (границы,
/// сортировку) ядро уже проверило — клиент доверяет проводу.
fn read_entity_tail(r: &mut api::Reader<'_>) -> Result<Vec<Entity>, LinkError> {
    let count = r.read_u8().map_err(|_| LinkError::BadFrame("ent_count"))?;
    let mut entities = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = r.read_u8().map_err(|_| LinkError::BadFrame("ent_kind"))?;
        let offset = r
            .read_u16()
            .map_err(|_| LinkError::BadFrame("ent_offset"))?;
        let len = r.read_u16().map_err(|_| LinkError::BadFrame("ent_len"))?;
        let value_len = r
            .read_u8()
            .map_err(|_| LinkError::BadFrame("ent_value_len"))? as usize;
        let value = r
            .read_bytes(value_len)
            .map_err(|_| LinkError::BadFrame("ent_value"))?;
        let Some(kind) = EntityKind::from_wire(kind) else {
            continue; // kind из будущего ядра — пропускаем запись
        };
        entities.push(Entity {
            kind,
            offset,
            len,
            value: String::from_utf8_lossy(value).into_owned(),
        });
    }
    Ok(entities)
}

/// Закодировать entity tail (§4.3). Список обязан пройти
/// [`entities_valid`] — ui строит его парсером разметки, mock и ядро
/// валидируют повторно; здесь — только раскладка байтов.
fn put_entity_tail(payload: &mut Vec<u8>, entities: &[Entity]) {
    debug_assert!(entities.len() <= 32, "ent_count u8");
    payload.push(entities.len() as u8);
    for entity in entities {
        payload.push(entity.kind.to_wire());
        api::put::u16(payload, entity.offset);
        api::put::u16(payload, entity.len);
        payload.push(entity.value.len() as u8);
        payload.extend_from_slice(entity.value.as_bytes());
    }
}

/// LIST_CONTACTS: count × {cid u32, flags u8, last_seen i64, name str16}.
fn decode_contacts(payload: &[u8]) -> Result<Vec<Contact>, LinkError> {
    let mut r = api::Reader::new(payload);
    let count = r.read_u32().map_err(|_| LinkError::BadFrame("count"))?;
    let mut contacts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let cid = r.read_u32().map_err(|_| LinkError::BadFrame("cid"))?;
        let flags = r.read_u8().map_err(|_| LinkError::BadFrame("flags"))?;
        let _last_seen = r.read_i64().map_err(|_| LinkError::BadFrame("last_seen"))?;
        let name = r.read_str16().map_err(|_| LinkError::BadFrame("name"))?;
        // avatar_id в LIST_CONTACTS нет — узнаём из PROFILE/GET_AVATAR_ID.
        contacts.push(Contact {
            cid: Cid(i64::from(cid)),
            name: String::from_utf8_lossy(name).into_owned(),
            online: flags & 0b01 != 0,
            blocked: flags & 0b10 != 0,
            avatar_id: None,
        });
    }
    Ok(contacts)
}

/// Событие → LinkEvent; не используемые клиентом события (реакции,
/// звонки, аватары, файлы) — None, до своих спринтов.
fn decode_event(header: &Header, payload: &[u8]) -> Option<LinkEvent> {
    let mut r = api::Reader::new(payload);
    match api::Event::from_u16(header.id)? {
        api::Event::MessageReceived => {
            let cid = r.read_u32().ok()?;
            let mut msg_id = [0u8; 16];
            msg_id.copy_from_slice(r.read_bytes(16).ok()?);
            let ts_ms = r.read_u64().ok()? as i64;
            let name = r.read_str16().ok()?;
            let text = r.read_str16().ok()?;
            // Entity tail только у ядра 2.0+ (§4.3); версия зонда до
            // события не доходит (decode_event её не знает), поэтому
            // решаем по остатку: байтов нет — ядро 1.x, есть — tail;
            // обрезанный tail — событие пропускаем, поток не рвём.
            let entities = if r.has_more() {
                read_entity_tail(&mut r).ok()?
            } else {
                Vec::new()
            };
            Some(LinkEvent::MessageReceived {
                cid: Cid(i64::from(cid)),
                name: String::from_utf8_lossy(name).into_owned(),
                msg_id: CoreMsgId(msg_id),
                ts_ms,
                text: String::from_utf8_lossy(text).into_owned(),
                entities,
            })
        }
        api::Event::MessageDelivered => {
            let cid = r.read_u32().ok()?;
            let mut msg_id = [0u8; 16];
            msg_id.copy_from_slice(r.read_bytes(16).ok()?);
            Some(LinkEvent::MessageDelivered {
                cid: Cid(i64::from(cid)),
                msg_id: CoreMsgId(msg_id),
            })
        }
        api::Event::Presence => {
            let cid = r.read_u32().ok()?;
            let online = r.read_u8().ok()?;
            let last_seen_ms = r.read_i64().ok()?;
            Some(LinkEvent::Presence {
                cid: Cid(i64::from(cid)),
                online: online != 0,
                last_seen_ms,
            })
        }
        api::Event::ContactJoined => {
            let cid = r.read_u32().ok()?;
            let name = r.read_str16().ok()?;
            Some(LinkEvent::ContactJoined {
                cid: Cid(i64::from(cid)),
                name: String::from_utf8_lossy(name).into_owned(),
            })
        }
        api::Event::ContactProfile => {
            let cid = r.read_u32().ok()?;
            let name = r.read_str16().ok()?;
            let mut avatar_id = [0u8; 16];
            avatar_id.copy_from_slice(r.read_bytes(16).ok()?);
            Some(LinkEvent::ContactProfile {
                cid: Cid(i64::from(cid)),
                name: String::from_utf8_lossy(name).into_owned(),
                avatar_id: CoreMsgId(avatar_id),
            })
        }
        api::Event::BlockedByPeer => {
            let cid = r.read_u32().ok()?;
            Some(LinkEvent::BlockedByPeer {
                cid: Cid(i64::from(cid)),
            })
        }
        // CONTACT_CARD (12, API 2.2+): [cid u32 — чат-отправитель]
        // [name str16][bundle 64][addr str16]; addr пуст, если неизвестен.
        api::Event::ContactCard => {
            let cid = r.read_u32().ok()?;
            let name = r.read_str16().ok()?;
            let mut bundle = [0u8; 64];
            bundle.copy_from_slice(r.read_bytes(64).ok()?);
            let addr = r.read_str16().ok()?;
            Some(LinkEvent::ContactCard {
                cid: Cid(i64::from(cid)),
                name: String::from_utf8_lossy(name).into_owned(),
                bundle,
                addr: String::from_utf8_lossy(addr).into_owned(),
            })
        }
        // CALL_STATE (5, §4.6/§5): [cid u32][call_id 16][state u8].
        api::Event::CallState => {
            let cid = r.read_u32().ok()?;
            let mut call_id = [0u8; 16];
            call_id.copy_from_slice(r.read_bytes(16).ok()?);
            let state = match r.read_u8().ok()? {
                1 => CallSignal::Offer,
                2 => CallSignal::Accept,
                3 => CallSignal::Reject,
                4 => CallSignal::End,
                // Код из будущего ядра — событие выбрасываем, поток не рвём.
                _ => return None,
            };
            Some(LinkEvent::CallState {
                cid: Cid(i64::from(cid)),
                call_id,
                state,
            })
        }
        // REACTION (4, §5): [cid u32][msg_id 16][op u8 (1 add, 2 remove)]
        // [codepoint u32][ts_ms u64]. Незнакомый op или кодпоинт вне
        // Unicode — событие выбрасываем, поток не рвём.
        api::Event::Reaction => {
            let cid = r.read_u32().ok()?;
            let mut msg_id = [0u8; 16];
            msg_id.copy_from_slice(r.read_bytes(16).ok()?);
            let add = match r.read_u8().ok()? {
                1 => true,
                2 => false,
                _ => return None,
            };
            let code = r.read_u32().ok()?;
            let emoji = char::from_u32(code)?;
            let ts_ms = r.read_u64().ok()? as i64;
            Some(LinkEvent::Reaction {
                cid: Cid(i64::from(cid)),
                msg_id: CoreMsgId(msg_id),
                add,
                emoji,
                ts_ms,
            })
        }
        // FILE_RECEIVED (7, §4.5): [cid u32][file_id 16][size u32]
        // [name str16]. Файл уже на диске ядра (~/.stille-messenger/files/<hex>).
        api::Event::FileReceived => {
            let cid = r.read_u32().ok()?;
            let mut file_id = [0u8; 16];
            file_id.copy_from_slice(r.read_bytes(16).ok()?);
            let size = r.read_u32().ok()?;
            let name = r.read_str16().ok()?;
            Some(LinkEvent::FileReceived {
                cid: Cid(i64::from(cid)),
                file_id,
                size,
                name: String::from_utf8_lossy(name).into_owned(),
            })
        }
        // FILE_SENT (11, §4.5): [cid u32][file_id 16][size u32][name
        // str16] — все чанки подтверждены; size/name уже известны из
        // ответа SEND_FILE, не дублируем в событии модели.
        api::Event::FileSent => {
            let cid = r.read_u32().ok()?;
            let mut file_id = [0u8; 16];
            file_id.copy_from_slice(r.read_bytes(16).ok()?);
            let _size = r.read_u32().ok()?;
            let _name = r.read_str16().ok()?;
            Some(LinkEvent::FileSent {
                cid: Cid(i64::from(cid)),
                file_id,
            })
        }
        // AVATAR_RECEIVED (6, §4.4): [cid u32][avatar_id 16][data str16].
        api::Event::AvatarReceived => {
            let cid = r.read_u32().ok()?;
            let mut avatar_id = [0u8; 16];
            avatar_id.copy_from_slice(r.read_bytes(16).ok()?);
            let data = r.read_str16().ok()?.to_vec();
            Some(LinkEvent::AvatarReceived {
                cid: Cid(i64::from(cid)),
                avatar_id,
                data,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_link::take_events;
    use tokio::net::{UnixListener, UnixStream as TokioUnixStream};

    /// Путь сокета в /tmp, уникальный для теста.
    fn temp_socket(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "stille-messenger-test-{tag}-{}.sock",
            std::process::id()
        ))
    }

    fn response_frame(id: u16, request_id: u32, status: Status, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        api::encode_frame(
            &mut frame,
            Header {
                kind: Kind::Response,
                id,
                request_id,
                status,
                payload_len: payload.len() as u32,
            },
            payload,
        );
        frame
    }

    fn event_frame(event: api::Event, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        api::encode_frame(
            &mut frame,
            Header {
                kind: Kind::Event,
                id: event as u16,
                request_id: 0,
                status: Status::Ok,
                payload_len: payload.len() as u32,
            },
            payload,
        );
        frame
    }

    async fn read_frame(stream: &mut TokioUnixStream) -> (Header, Vec<u8>) {
        let mut buf = [0u8; api::HEADER_LEN];
        stream
            .read_exact(&mut buf)
            .await
            .expect("заголовок прочитан");
        let header = api::decode_header(&buf).expect("кадр корректен");
        let mut payload = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 {
            stream
                .read_exact(&mut payload)
                .await
                .expect("payload прочитан");
        }
        (header, payload)
    }

    /// Убрать сокет, если тест до него не добрался.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Ответить на зондирующий GET_VERSION: фейковое ядро версии 2.2.0.
    /// Реальный запрос проверяется на версионность отдельно.
    async fn answer_version(stream: &mut TokioUnixStream, major: u16) {
        let (header, payload) = read_frame(stream).await;
        assert_eq!(header.id, Method::GetVersion as u16, "первый запрос — зонд");
        assert!(payload.is_empty(), "GET_VERSION без payload");
        let mut answer = Vec::new();
        api::put::u16(&mut answer, major);
        api::put::u16(&mut answer, 0);
        api::put::u16(&mut answer, 0);
        stream
            .write_all(&response_frame(
                Method::GetVersion as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("версия отправлена");
    }

    #[tokio::test]
    async fn contacts_roundtrip_decodes_typed_list() {
        let path = temp_socket("contacts");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");

        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let events = take_events(CoreLink::events(Arc::clone(&link)), 1);

        let (mut stream, _) = listener.accept().await.expect("клиент подключился");
        // Запрос в отдельной задаче: однопоточный рантайм теста —
        // главный таск обслуживает фейковое ядро.
        let reply = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { CoreLink::execute(&*link, Request::Contacts).await })
        };

        answer_version(&mut stream, 2).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.kind, Kind::Request);
        assert_eq!(header.id, Method::ListContacts as u16);
        assert_eq!(payload.len(), 0, "LIST_CONTACTS без payload");

        let mut answer = Vec::new();
        api::put::u32(&mut answer, 1);
        api::put::u32(&mut answer, 5);
        api::put::u8(&mut answer, 0b01);
        api::put::i64(&mut answer, 1234);
        api::put::str16(&mut answer, "Аня Б").expect("str16");
        stream
            .write_all(&response_frame(
                Method::ListContacts as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ отправлен");

        assert_eq!(
            reply.await.expect("задача не паникует"),
            Ok(OkReply::Contacts(vec![Contact {
                cid: Cid(5),
                name: "Аня Б".into(),
                online: true,
                blocked: false,
                avatar_id: None,
            }]))
        );
        let events = events.await;
        assert_eq!(events.front(), Some(&LinkEvent::Resync), "коннект = ресинк");
        cleanup(&path);
    }

    #[tokio::test]
    async fn send_message_binds_core_id_and_maps_kernel_error() {
        let path = temp_socket("send");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));

        let (mut stream, _) = listener.accept().await.expect("клиент подключился");
        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::SendMsg {
                        cid: Cid(3),
                        text: "привет".into(),
                        entities: Vec::new(),
                    },
                )
                .await
            })
        };

        // Зонд версии: демон 2.2.0 — SEND_MESSAGE обязан нести entity tail
        // уже в первом запросе.
        answer_version(&mut stream, 2).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.id, Method::SendMessage as u16);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(3));
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok("привет")
        );
        assert_eq!(r.read_u8(), Ok(0), "entity tail: ent_count = 0");
        assert!(!r.has_more(), "после tail payload исчерпан");
        let mut answer = Vec::new();
        answer.extend_from_slice(&[0xAB; 16]);
        api::put::u8(&mut answer, 1);
        stream
            .write_all(&response_frame(
                Method::SendMessage as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ отправлен");
        assert_eq!(
            task.await.expect("задача не паникует"),
            Ok(OkReply::Sent {
                msg_id: CoreMsgId([0xAB; 16]),
                queued: true
            }),
            "queued=1 — сообщение в outbox ядра"
        );

        // Второй запрос — зонд не повторяется, tail ставится сразу,
        // а ядро отвечает машинным кодом.
        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::SendMsg {
                        cid: Cid(3),
                        text: "ещё".into(),
                        entities: Vec::new(),
                    },
                )
                .await
            })
        };
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.id, Method::SendMessage as u16, "зонд единственный");
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(3));
        assert!(r.read_str16().is_ok());
        assert_eq!(r.read_u8(), Ok(0), "tail сразу");
        stream
            .write_all(&response_frame(
                Method::SendMessage as u16,
                header.request_id,
                Status::ContactBlocked,
                &[],
            ))
            .await
            .expect("ответ отправлен");
        let reply = task.await.expect("задача не паникует");
        assert_eq!(reply, Err(LinkError::Kernel(Status::ContactBlocked)));
        assert_eq!(
            reply.unwrap_err().user_text(),
            "Контакт заблокирован. Разблокируйте, чтобы писать."
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn kernel_events_reach_the_stream() {
        let path = temp_socket("events");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let events = take_events(CoreLink::events(Arc::clone(&link)), 3);

        let (mut stream, _) = listener.accept().await.expect("клиент подключился");

        let mut presence = Vec::new();
        api::put::u32(&mut presence, 7);
        api::put::u8(&mut presence, 1);
        api::put::i64(&mut presence, 42);
        stream
            .write_all(&event_frame(api::Event::Presence, &presence))
            .await
            .expect("событие");

        let mut received = Vec::new();
        api::put::u32(&mut received, 7);
        received.extend_from_slice(&[0x11; 16]);
        api::put::u64(&mut received, 1_700_000_000_000);
        api::put::str16(&mut received, "Аня").expect("str16");
        api::put::str16(&mut received, "привет, мир").expect("str16");
        stream
            .write_all(&event_frame(api::Event::MessageReceived, &received))
            .await
            .expect("событие");

        let events = events.await;
        assert_eq!(events[0], LinkEvent::Resync, "коннект = ресинк");
        assert_eq!(
            events[1],
            LinkEvent::Presence {
                cid: Cid(7),
                online: true,
                last_seen_ms: 42
            }
        );
        assert_eq!(
            events[2],
            LinkEvent::MessageReceived {
                cid: Cid(7),
                name: "Аня".into(),
                msg_id: CoreMsgId([0x11; 16]),
                ts_ms: 1_700_000_000_000,
                text: "привет, мир".into(),
                entities: Vec::new(),
            }
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn disconnect_drains_pending_and_resyncs() {
        let path = temp_socket("reconn");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let events = take_events(CoreLink::events(Arc::clone(&link)), 3);

        // Коннект 1: обычный обмен.
        let (mut stream, _) = listener.accept().await.expect("коннект 1");
        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { CoreLink::execute(&*link, Request::Contacts).await })
        };
        answer_version(&mut stream, 2).await;
        let (header, _) = read_frame(&mut stream).await;
        let mut answer = Vec::new();
        api::put::u32(&mut answer, 0);
        stream
            .write_all(&response_frame(
                Method::ListContacts as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ");
        assert_eq!(
            task.await.expect("задача не паникует"),
            Ok(OkReply::Contacts(vec![]))
        );

        // Запрос в полёте — и ядро умирает, не ответив.
        let task2 = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { CoreLink::execute(&*link, Request::Contacts).await })
        };
        let _ = read_frame(&mut stream).await; // запрос дошёл по коннекту 1
        drop(stream); // ...ответа не будет
        assert_eq!(
            task2.await.expect("задача не паникует"),
            Err(LinkError::Disconnected),
            "зависший запрос получает Disconnected, а не висит вечно"
        );

        // Реконнект: Resync, дальше запрос ходит по новому соединению —
        // зонд версии повторяется, между соединениями демон могли заменить.
        let (mut stream2, _) = listener.accept().await.expect("коннект 2 — ресинк");
        let task3 = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { CoreLink::execute(&*link, Request::Contacts).await })
        };
        answer_version(&mut stream2, 2).await;
        let (header, _) = read_frame(&mut stream2).await;
        let mut answer = Vec::new();
        api::put::u32(&mut answer, 1);
        api::put::u32(&mut answer, 9);
        api::put::u8(&mut answer, 0);
        api::put::i64(&mut answer, 0);
        api::put::str16(&mut answer, "Борис").expect("str16");
        stream2
            .write_all(&response_frame(
                Method::ListContacts as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ");
        assert_eq!(
            task3.await.expect("задача не паникует"),
            Ok(OkReply::Contacts(vec![Contact {
                cid: Cid(9),
                name: "Борис".into(),
                online: false,
                blocked: false,
                avatar_id: None,
            }]))
        );

        let events = events.await;
        assert_eq!(events[0], LinkEvent::Resync, "первый коннект");
        assert_eq!(events[1], LinkEvent::Down, "обрыв первого");
        assert_eq!(events[2], LinkEvent::Resync, "ресинк после реконнекта");
        cleanup(&path);
    }

    #[tokio::test]
    async fn accept_invite_block_unblock_payloads_on_wire() {
        let path = temp_socket("meet");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));
        let (mut stream, _) = listener.accept().await.expect("коннект");

        // ACCEPT_INVITE: code str16 + my_name str16 (пустое — ядро подставит «друг»).
        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::Add {
                        code: "stille://i/AbCdEf".into(),
                    },
                )
                .await
            })
        };
        answer_version(&mut stream, 2).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.id, Method::AcceptInvite as u16);
        let mut r = api::Reader::new(&payload);
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok("stille://i/AbCdEf")
        );
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok(""),
            "my_name пустой: имя задаёт держатель кода у себя"
        );
        assert!(!r.has_more(), "строгий разбор payload");
        let mut answer = Vec::new();
        api::put::u32(&mut answer, 12);
        stream
            .write_all(&response_frame(
                Method::AcceptInvite as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ");
        assert_eq!(
            task.await.expect("задача не паникует"),
            Ok(OkReply::Added { cid: Cid(12) })
        );

        // BLOCK_CONTACT: только cid u32; ответ пуст.
        for (method, request) in [
            (Method::BlockContact, Request::Block { cid: Cid(12) }),
            (Method::UnblockContact, Request::Unblock { cid: Cid(12) }),
        ] {
            let task = {
                let link = Arc::clone(&link);
                let request = request.clone();
                tokio::spawn(async move { CoreLink::execute(&*link, request).await })
            };
            let (header, payload) = read_frame(&mut stream).await;
            assert_eq!(header.id, method as u16);
            assert_eq!(payload, 12u32.to_le_bytes(), "payload = cid u32");
            stream
                .write_all(&response_frame(
                    method as u16,
                    header.request_id,
                    Status::Ok,
                    &[],
                ))
                .await
                .expect("ответ");
            assert_eq!(task.await.expect("задача не паникует"), Ok(OkReply::Done));
        }
        cleanup(&path);
    }

    /// GET_HISTORY у API 2.x: каждая запись несёт entity tail (даже
    /// пустой ent_count = 0), флаги mine/delivered читаются из bits.
    #[tokio::test]
    async fn history_reply_from_api2_decodes_entity_tails() {
        let path = temp_socket("history2");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));
        let (mut stream, _) = listener.accept().await.expect("коннект");

        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::History {
                        cid: Cid(5),
                        limit: 50,
                    },
                )
                .await
            })
        };
        answer_version(&mut stream, 2).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.id, Method::GetHistory as u16);
        assert_eq!(
            payload,
            [5, 0, 0, 0, 50, 0, 0, 0],
            "payload = cid u32 + limit u32"
        );

        // total=3, count=2; запись 1 — моя доставленная с сущностью,
        // запись 2 — чужая с пустым tail.
        let mut answer = Vec::new();
        api::put::u32(&mut answer, 3);
        api::put::u32(&mut answer, 2);
        api::put::u8(&mut answer, 0b11);
        answer.extend_from_slice(&[0x01; 16]);
        api::put::u64(&mut answer, 1_700_000_000_000);
        api::put::str16(&mut answer, "моё старое").expect("str16");
        api::put::u8(&mut answer, 1); // ent_count = 1
        api::put::u8(&mut answer, 1); // kind
        api::put::u16(&mut answer, 0); // offset
        api::put::u16(&mut answer, 4); // len
        api::put::u8(&mut answer, 4); // value_len
        answer.extend_from_slice(b"bold");
        api::put::u8(&mut answer, 0b00);
        answer.extend_from_slice(&[0x02; 16]);
        api::put::u64(&mut answer, 1_700_000_005_000);
        api::put::str16(&mut answer, "чужое").expect("str16");
        api::put::u8(&mut answer, 0); // ent_count = 0
        stream
            .write_all(&response_frame(
                Method::GetHistory as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ отправлен");
        assert_eq!(
            task.await.expect("задача не паникует"),
            Ok(OkReply::History {
                total: 3,
                records: vec![
                    HistoryMsg {
                        mine: true,
                        delivered: true,
                        msg_id: CoreMsgId([0x01; 16]),
                        ts_ms: 1_700_000_000_000,
                        text: "моё старое".into(),
                        // Сущность с провода разобрана (S14); value у
                        // жирного неконтрактно, но семантику проверяет
                        // ядро — клиент доверяет проводу.
                        entities: vec![Entity {
                            kind: EntityKind::Bold,
                            offset: 0,
                            len: 4,
                            value: "bold".into(),
                        }],
                    },
                    HistoryMsg {
                        mine: false,
                        delivered: false,
                        msg_id: CoreMsgId([0x02; 16]),
                        ts_ms: 1_700_000_005_000,
                        text: "чужое".into(),
                        entities: Vec::new(),
                    },
                ],
            }),
            "tails разобраны в entities, поля на местах"
        );
        cleanup(&path);
    }

    /// GET_HISTORY у API 1.x: ядро шлёт записи без tail — разбор идёт
    /// строго, лишний байт был бы BadFrame.
    #[tokio::test]
    async fn history_reply_from_api1_decodes_bare_records() {
        let path = temp_socket("history1");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));
        let (mut stream, _) = listener.accept().await.expect("коннект");

        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::History {
                        cid: Cid(5),
                        limit: 50,
                    },
                )
                .await
            })
        };
        answer_version(&mut stream, 1).await;
        let (header, _) = read_frame(&mut stream).await;

        let mut answer = Vec::new();
        api::put::u32(&mut answer, 1);
        api::put::u32(&mut answer, 1);
        api::put::u8(&mut answer, 0b00);
        answer.extend_from_slice(&[0x02; 16]);
        api::put::u64(&mut answer, 1_700_000_005_000);
        api::put::str16(&mut answer, "старое ядро").expect("str16");
        stream
            .write_all(&response_frame(
                Method::GetHistory as u16,
                header.request_id,
                Status::Ok,
                &answer,
            ))
            .await
            .expect("ответ отправлен");
        assert_eq!(
            task.await.expect("задача не паникует"),
            Ok(OkReply::History {
                total: 1,
                records: vec![HistoryMsg {
                    mine: false,
                    delivered: false,
                    msg_id: CoreMsgId([0x02; 16]),
                    ts_ms: 1_700_000_005_000,
                    text: "старое ядро".into(),
                    entities: Vec::new(),
                }],
            }),
            "записи 1.x без tail разбираются"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn responses_route_by_request_id_even_out_of_order() {
        let path = temp_socket("ooo");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));

        let (mut stream, _) = listener.accept().await.expect("коннект");

        // Два запроса параллельно; ответы ядро шлёт в обратном порядке.
        let first = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { CoreLink::execute(&*link, Request::Contacts).await })
        };
        let second = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::Invite {
                        relay: false,
                        addr: String::new(),
                    },
                )
                .await
            })
        };

        // Оба первых запроса увидели неизвестную версию — зондов два.
        answer_version(&mut stream, 2).await;
        answer_version(&mut stream, 2).await;
        let (h1, _) = read_frame(&mut stream).await;
        let (h2, _) = read_frame(&mut stream).await;

        let mut code = Vec::new();
        api::put::str16(&mut code, "stille://i/def").expect("str16");
        // Сначала ответ на ВТОРОЙ (h2), потом на ПЕРВЫЙ (h1).
        stream
            .write_all(&response_frame(h2.id, h2.request_id, Status::Ok, &code))
            .await
            .expect("ответ 2");
        let mut empty = Vec::new();
        api::put::u32(&mut empty, 0); // LIST_CONTACTS: count = 0
        stream
            .write_all(&response_frame(h1.id, h1.request_id, Status::Ok, &empty))
            .await
            .expect("ответ 1");

        assert_eq!(
            second.await.expect("задача не паникует"),
            Ok(OkReply::Code("stille://i/def".into()))
        );
        assert_eq!(
            first.await.expect("задача не паникует"),
            Ok(OkReply::Contacts(vec![]))
        );
        cleanup(&path);
    }

    /// Запрос поиска (S11, API 2.1+): [cid u32][query str16][limit u32]
    /// [offset u32]; глобальный скоп — без cid-префикса (§4.3).
    #[test]
    fn search_request_encodes_chat_and_global_scopes() {
        let (method, payload) = encode_request(
            &Request::Search {
                cid: Some(Cid(7)),
                query: "привет".into(),
                limit: 50,
                offset: 100,
            },
            true,
        );
        assert_eq!(method, Method::SearchMessage);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(7));
        assert_eq!(
            r.read_str16()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            Ok("привет".to_owned())
        );
        assert_eq!(r.read_u32(), Ok(50));
        assert_eq!(r.read_u32(), Ok(100));
        assert!(!r.has_more(), "строгий разбор: payload прочитан до конца");

        let (method, payload) = encode_request(
            &Request::Search {
                cid: None,
                query: "хвост".into(),
                limit: 0,
                offset: 0,
            },
            true,
        );
        assert_eq!(method, Method::GlobalSearchMessage);
        let mut r = api::Reader::new(&payload);
        assert_eq!(
            r.read_str16()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            Ok("хвост".to_owned()),
            "глобальный скоп: cid на проводе нет"
        );
        assert_eq!(r.read_u32(), Ok(0));
        assert_eq!(r.read_u32(), Ok(0));
        assert!(!r.has_more());
    }

    /// Запись ответа поиска: [flags][msg_id 16][ts u64][body str16]
    /// [entity tail]; у глобального — [cid u32] впереди.
    fn push_search_record(payload: &mut Vec<u8>, cid: Option<u32>, flags: u8, body: &str) {
        if let Some(cid) = cid {
            api::put::u32(payload, cid);
        }
        api::put::u8(payload, flags);
        payload.extend_from_slice(&[0xAB; 16]);
        api::put::u64(payload, 1_700_000_000_123);
        api::put::str16(payload, body).expect("str16");
        // Entity tail: одна ссылка (kind 8) — проверка, что tail
        // разбирается, а не съедает следующую запись.
        api::put::u8(payload, 1);
        api::put::u8(payload, 8);
        api::put::u16(payload, 0);
        api::put::u16(payload, 4);
        api::put::u8(payload, 4);
        payload.extend_from_slice(b"http");
    }

    /// Ответ чатового поиска: флаги mine/delivered/fuzzy разложены,
    /// entity tail пропущен, cid остаётся нулевым — заполнит roundtrip.
    #[test]
    fn search_reply_decodes_flags_and_entity_tail() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 2); // total
        api::put::u32(&mut payload, 2); // count
        push_search_record(&mut payload, None, 0b101, "точное и моё"); // mine+fuzzy
        push_search_record(&mut payload, None, 0b010, "доставленное");

        let reply = decode_reply(
            Method::SearchMessage,
            RawReply {
                status: Status::Ok,
                payload,
            },
            true,
        );
        let Ok(OkReply::Search { total, hits }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].mine && hits[0].fuzzy && !hits[0].delivered);
        assert!(!hits[1].mine && hits[1].delivered && !hits[1].fuzzy);
        assert_eq!(hits[1].body, "доставленное");
        assert_eq!(hits[1].ts_ms, 1_700_000_000_123);
    }

    /// Ответ глобального поиска: cid перед флагами каждой записи.
    #[test]
    fn global_search_reply_decodes_cid_per_record() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 5); // total
        api::put::u32(&mut payload, 2); // count
        push_search_record(&mut payload, Some(3), 0b000, "у Ани");
        push_search_record(&mut payload, Some(9), 0b001, "у Бориса");

        let reply = decode_reply(
            Method::GlobalSearchMessage,
            RawReply {
                status: Status::Ok,
                payload,
            },
            true,
        );
        let Ok(OkReply::Search { total, hits }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(total, 5);
        assert_eq!(hits[0].cid, Cid(3));
        assert_eq!(hits[1].cid, Cid(9));
        assert!(hits[1].mine);
    }

    /// Обрезанный entity tail — BadFrame, а не паника/усечение.
    #[test]
    fn search_reply_with_truncated_tail_is_bad_frame() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 1);
        api::put::u32(&mut payload, 1);
        push_search_record(&mut payload, None, 0, "тело");
        payload.truncate(payload.len() - 2); // tail недоеден

        let reply = decode_reply(
            Method::SearchMessage,
            RawReply {
                status: Status::Ok,
                payload,
            },
            true,
        );
        assert!(matches!(reply, Err(LinkError::BadFrame(_))));
    }

    /// Закрепы (S12, §4.8): раскладки запросов и ответов по контракту.
    #[test]
    fn pins_codec_matches_contract() {
        // PIN_CHAT: [cid u32].
        let (method, payload) = encode_request(&Request::PinChat { cid: Cid(7) }, true);
        assert_eq!(method, Method::PinChat);
        assert_eq!(payload, 7u32.to_le_bytes());

        // PIN_MESSAGE: [cid u32][msg_id 16].
        let msg_id = CoreMsgId([0xAA; 16]);
        let (method, payload) = encode_request(
            &Request::PinMessage {
                cid: Cid(7),
                msg_id,
            },
            true,
        );
        assert_eq!(method, Method::PinMessage);
        assert_eq!(&payload[..4], 7u32.to_le_bytes());
        assert_eq!(&payload[4..], [0xAA; 16]);

        // UNPIN_CHAT / GET_PINNED_CHAT: пустые.
        assert_eq!(
            encode_request(&Request::UnpinChat, true).1,
            Vec::<u8>::new()
        );
        assert_eq!(
            encode_request(&Request::PinnedChat, true).1,
            Vec::<u8>::new()
        );

        // GET_MESSAGE: [msg_id 16].
        let (method, payload) = encode_request(&Request::GetMessage { msg_id }, true);
        assert_eq!(method, Method::GetMessage);
        assert_eq!(payload, [0xAA; 16]);
    }

    #[test]
    fn pinned_chat_reply_none_and_some() {
        let decode = |cid: u32| {
            decode_reply(
                Method::GetPinnedChat,
                RawReply {
                    status: Status::Ok,
                    payload: cid.to_le_bytes().to_vec(),
                },
                true,
            )
        };
        assert_eq!(
            decode(0).expect("ноль"),
            OkReply::PinnedChat { cid: None },
            "0 = закрепа нет"
        );
        assert_eq!(
            decode(7).expect("cid"),
            OkReply::PinnedChat { cid: Some(Cid(7)) }
        );
    }

    #[test]
    fn pinned_messages_reply_decodes_id_list() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 2);
        payload.extend_from_slice(&[1u8; 16]);
        payload.extend_from_slice(&[2u8; 16]);
        let reply = decode_reply(
            Method::GetPinnedMessages,
            RawReply {
                status: Status::Ok,
                payload,
            },
            true,
        );
        let Ok(OkReply::PinnedMessages { ids }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(ids, vec![CoreMsgId([1; 16]), CoreMsgId([2; 16])]);
    }

    /// GET_MESSAGE: флаги + обязательные entity tail и список реакций
    /// (API 2.x) пропускаются, тело и сторона доезжают.
    #[test]
    fn get_message_reply_skips_entities_and_reactions() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7); // cid
        payload.push(0b11); // mine + delivered
        api::put::u64(&mut payload, 1_700_000_000_000);
        api::put::str16(&mut payload, "тело закрепа").expect("str16");
        payload.push(0); // ent_count: разметки нет
        api::put::u32(&mut payload, 1); // одна реакция
        payload.push(1); // react flags: моя
        api::put::u32(&mut payload, 0x2764); // ❤
        api::put::u64(&mut payload, 1_700_000_000_100);

        let reply = decode_reply(
            Method::GetMessage,
            RawReply {
                status: Status::Ok,
                payload,
            },
            true,
        );
        let Ok(OkReply::Message {
            cid,
            mine,
            delivered,
            text,
            ..
        }) = reply
        else {
            panic!("{reply:?}");
        };
        assert_eq!(cid, Cid(7));
        assert!(mine && delivered);
        assert_eq!(text, "тело закрепа");
    }

    /// Визитки и удаление чата (S13, §4.8): раскладки запросов.
    #[test]
    fn cards_and_delete_codec_matches_contract() {
        // DELETE_CHAT: [cid u32].
        let (method, payload) = encode_request(&Request::DeleteChat { cid: Cid(7) }, true);
        assert_eq!(method, Method::DeleteChat);
        assert_eq!(payload, 7u32.to_le_bytes());

        // SEND_CONTACT чужой карточки: [to_cid u32][contact_cid u32].
        let (method, payload) = encode_request(
            &Request::SendContact {
                to: Cid(3),
                contact: Some(Cid(9)),
            },
            true,
        );
        assert_eq!(method, Method::SendContact);
        assert_eq!(&payload[..4], 3u32.to_le_bytes());
        assert_eq!(&payload[4..], 9u32.to_le_bytes());

        // Своя карточка — contact_cid = 0.
        let (_, payload) = encode_request(
            &Request::SendContact {
                to: Cid(3),
                contact: None,
            },
            true,
        );
        assert_eq!(&payload[4..], 0u32.to_le_bytes());
    }

    /// CONTACT_CARD (событие 12): [cid][name str16][bundle 64][addr str16];
    /// пустой адрес остаётся пустым, cid — чат-отправитель.
    #[test]
    fn contact_card_event_decodes() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 5);
        api::put::str16(&mut payload, "Вера").expect("str16");
        payload.extend_from_slice(&[0x42; 64]);
        api::put::str16(&mut payload, "host:7777").expect("str16");
        let header = Header {
            kind: Kind::Event,
            id: api::Event::ContactCard as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        let Some(LinkEvent::ContactCard {
            cid,
            name,
            bundle,
            addr,
        }) = decode_event(&header, &payload)
        else {
            panic!("событие не разобрано")
        };
        assert_eq!(cid, Cid(5), "cid — отправитель визитки");
        assert_eq!(name, "Вера");
        assert_eq!(bundle, [0x42; 64]);
        assert_eq!(addr, "host:7777");

        // Адрес неизвестен — пустая строка, а не ошибка.
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 5);
        api::put::str16(&mut payload, "Вера").expect("str16");
        payload.extend_from_slice(&[0x42; 64]);
        api::put::str16(&mut payload, "").expect("str16");
        let Some(LinkEvent::ContactCard { addr, .. }) = decode_event(&header, &payload) else {
            panic!("событие не разобрано")
        };
        assert_eq!(addr, "");

        // Урезанный bundle — событие выбрасывается, поток не рвём.
        let mut short = Vec::new();
        api::put::u32(&mut short, 5);
        api::put::str16(&mut short, "Вера").expect("str16");
        short.extend_from_slice(&[0x42; 10]);
        assert_eq!(decode_event(&header, &short), None);
    }

    /// SET_NAME (§4.4): [name str16].
    #[test]
    fn set_name_codec_matches_contract() {
        let (method, payload) = encode_request(
            &Request::SetName {
                name: "Михаил".into(),
            },
            true,
        );
        assert_eq!(method, Method::SetName);
        let mut expect = Vec::new();
        api::put::str16(&mut expect, "Михаил").expect("str16");
        assert_eq!(payload, expect);
    }

    /// Звонки (S7, §4.6): раскладки запросов, ответ START_CALL с call_id,
    /// событие CALL_STATE со всеми стадиями.
    #[test]
    fn calls_codec_matches_contract() {
        let (method, payload) = encode_request(&Request::StartCall { cid: Cid(7) }, true);
        assert_eq!(method, Method::StartCall);
        assert_eq!(payload, 7u32.to_le_bytes());
        let (method, payload) = encode_request(&Request::AnswerCall, true);
        assert_eq!((method, payload.len()), (Method::AnswerCall, 0));
        let (method, payload) = encode_request(&Request::EndCall, true);
        assert_eq!((method, payload.len()), (Method::EndCall, 0));

        // Ответ START_CALL: [call_id 16].
        let raw = RawReply {
            status: Status::Ok,
            payload: vec![0xAB; 16],
        };
        let reply = decode_reply(Method::StartCall, raw, true);
        assert_eq!(
            reply,
            Ok(OkReply::CallStarted {
                call_id: [0xAB; 16]
            })
        );
        // Короткий ответ — BadFrame, не паника.
        let raw = RawReply {
            status: Status::Ok,
            payload: vec![0xAB; 8],
        };
        assert!(matches!(
            decode_reply(Method::StartCall, raw, true),
            Err(LinkError::BadFrame(_))
        ));

        // CALL_STATE: [cid u32][call_id 16][state u8].
        for (code, signal) in [
            (1u8, CallSignal::Offer),
            (2, CallSignal::Accept),
            (3, CallSignal::Reject),
            (4, CallSignal::End),
        ] {
            let mut payload = Vec::new();
            api::put::u32(&mut payload, 5);
            payload.extend_from_slice(&[0x77; 16]);
            payload.push(code);
            let header = Header {
                kind: Kind::Event,
                id: api::Event::CallState as u16,
                request_id: 0,
                status: Status::Ok,
                payload_len: payload.len() as u32,
            };
            let Some(LinkEvent::CallState {
                cid,
                call_id,
                state,
            }) = decode_event(&header, &payload)
            else {
                panic!("событие не разобрано")
            };
            assert_eq!((cid, call_id, state), (Cid(5), [0x77; 16], signal));
        }

        // Код стадии из будущего ядра — событие выбрасывается.
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 5);
        payload.extend_from_slice(&[0x77; 16]);
        payload.push(9);
        let header = Header {
            kind: Kind::Event,
            id: api::Event::CallState as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        assert_eq!(decode_event(&header, &payload), None);
    }

    /// S14: SEND_MESSAGE с разметкой — tail на проводе по §4.3
    /// (счётчик, kind, offset/len, value ссылки).
    #[tokio::test]
    async fn send_message_encodes_entity_tail() {
        let path = temp_socket("send_ent");
        cleanup(&path);
        let listener = UnixListener::bind(&path).expect("сокет забинден");
        let link = Arc::new(UdsLink::with_config(
            path.clone(),
            Duration::from_millis(10),
            false,
        ));
        let _events = CoreLink::events(Arc::clone(&link));
        let (mut stream, _) = listener.accept().await.expect("коннект");

        let task = {
            let link = Arc::clone(&link);
            tokio::spawn(async move {
                CoreLink::execute(
                    &*link,
                    Request::SendMsg {
                        cid: Cid(3),
                        text: "жирная ссылка".into(),
                        entities: vec![
                            Entity::new(EntityKind::Bold, 0, 6),
                            Entity::link(7, 6, "https://shopot.dev".into()),
                        ],
                    },
                )
                .await
            })
        };
        answer_version(&mut stream, 2).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.id, Method::SendMessage as u16);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(3));
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok("жирная ссылка")
        );
        assert_eq!(r.read_u8(), Ok(2), "ent_count");
        // Bold: kind 1, offset 0, len 6, без value.
        assert_eq!(r.read_u8(), Ok(1));
        assert_eq!(r.read_u16(), Ok(0));
        assert_eq!(r.read_u16(), Ok(6));
        assert_eq!(r.read_u8(), Ok(0));
        // Link: kind 8, value = URL.
        assert_eq!(r.read_u8(), Ok(8));
        assert_eq!(r.read_u16(), Ok(7));
        assert_eq!(r.read_u16(), Ok(6));
        let url_len = r.read_u8().expect("value_len");
        assert_eq!(
            r.read_bytes(url_len as usize).unwrap(),
            b"https://shopot.dev"
        );
        assert!(!r.has_more(), "после tail payload исчерпан");
        task.abort();
        cleanup(&path);
    }

    /// S14: MESSAGE_RECEIVED ядра 2.x несёт entities; неизвестный kind
    /// (из будущего ядра) пропускается, событие живёт.
    #[test]
    fn message_received_decodes_entities_and_skips_unknown_kinds() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7);
        payload.extend_from_slice(&[0x11; 16]);
        api::put::u64(&mut payload, 1_700_000_000_000);
        api::put::str16(&mut payload, "Аня").expect("str16");
        api::put::str16(&mut payload, "смотри код").expect("str16");
        // tail: спойлер (kind 5) + неизвестный kind 42 + ссылка.
        api::put::u8(&mut payload, 3);
        api::put::u8(&mut payload, 5);
        api::put::u16(&mut payload, 0);
        api::put::u16(&mut payload, 6);
        api::put::u8(&mut payload, 0);
        api::put::u8(&mut payload, 42);
        api::put::u16(&mut payload, 1);
        api::put::u16(&mut payload, 2);
        api::put::u8(&mut payload, 3);
        payload.extend_from_slice(b"foo");
        api::put::u8(&mut payload, 8);
        api::put::u16(&mut payload, 7);
        api::put::u16(&mut payload, 3);
        api::put::u8(&mut payload, 8);
        payload.extend_from_slice(b"https://");

        let header = Header {
            kind: Kind::Event,
            id: api::Event::MessageReceived as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        let Some(LinkEvent::MessageReceived { entities, .. }) = decode_event(&header, &payload)
        else {
            panic!("событие не разобрано");
        };
        assert_eq!(
            entities,
            vec![
                Entity::new(EntityKind::Spoiler, 0, 6),
                Entity::link(7, 3, "https://".into()),
            ],
            "kind 42 пропущен, остальные разобраны"
        );
    }

    /// S14: обрезанный entity tail в событии — событие пропускаем,
    /// поток не рвётся.
    #[test]
    fn message_received_with_truncated_tail_is_dropped() {
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7);
        payload.extend_from_slice(&[0x11; 16]);
        api::put::u64(&mut payload, 1_700_000_000_000);
        api::put::str16(&mut payload, "Аня").expect("str16");
        api::put::str16(&mut payload, "текст").expect("str16");
        api::put::u8(&mut payload, 2); // заявлено 2 сущности, данных нет
        let header = Header {
            kind: Kind::Event,
            id: api::Event::MessageReceived as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        assert_eq!(decode_event(&header, &payload), None);
    }

    /// Реакции (S15): SET/REMOVE кодируются по §4.4; событие REACTION
    /// разбирается; незнакомый op и кодпоинт вне Unicode — выброс события.
    #[test]
    fn reaction_roundtrip_and_garbage() {
        let (method, payload) = encode_request(
            &Request::SetReaction {
                cid: Cid(7),
                msg_id: CoreMsgId([0x42; 16]),
                emoji: '👍',
            },
            true,
        );
        assert_eq!(method, Method::SetReaction);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(7));
        assert_eq!(r.read_bytes(16).unwrap(), &[0x42; 16]);
        assert_eq!(r.read_u32(), Ok('👍' as u32));
        assert!(!r.has_more());

        let (method, payload) = encode_request(
            &Request::RemoveReaction {
                cid: Cid(7),
                msg_id: CoreMsgId([0x42; 16]),
                emoji: '👍',
            },
            true,
        );
        assert_eq!((method, payload.len()), (Method::RemoveReaction, 24));

        // Событие add.
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7);
        payload.extend_from_slice(&[0x42; 16]);
        api::put::u8(&mut payload, 1);
        api::put::u32(&mut payload, '🔥' as u32);
        api::put::u64(&mut payload, 1_700_000_000_000);
        let header = Header {
            kind: Kind::Event,
            id: api::Event::Reaction as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        assert_eq!(
            decode_event(&header, &payload),
            Some(LinkEvent::Reaction {
                cid: Cid(7),
                msg_id: CoreMsgId([0x42; 16]),
                add: true,
                emoji: '🔥',
                ts_ms: 1_700_000_000_000,
            })
        );

        // op 9 из будущего ядра — событие выбрасываем.
        let mut bad = payload.clone();
        bad[20] = 9;
        assert_eq!(decode_event(&header, &bad), None);
        // Кодпоинт вне Unicode (surrogate) — выброс.
        let mut bad = payload;
        bad[21..25].copy_from_slice(&0xD800u32.to_le_bytes());
        assert_eq!(decode_event(&header, &bad), None);
    }

    /// S10: SEND_FILE — раскладка кадра и ответа; FILE_RECEIVED/FILE_SENT
    /// разбираются.
    #[test]
    fn file_wire_roundtrip() {
        let (method, payload) = encode_request(
            &Request::SendFile {
                cid: Cid(7),
                path: "/tmp/макет.pdf".into(),
            },
            true,
        );
        assert_eq!(method, Method::SendFile);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(7));
        assert_eq!(
            r.read_str16().map(|b| std::str::from_utf8(b).unwrap()),
            Ok("/tmp/макет.pdf")
        );
        assert!(!r.has_more());

        let mut raw = Vec::new();
        raw.extend_from_slice(&[0xF1; 16]);
        api::put::u32(&mut raw, 254_000);
        api::put::u32(&mut raw, 16);
        let reply = decode_reply(
            Method::SendFile,
            RawReply {
                status: Status::Ok,
                payload: raw,
            },
            true,
        );
        assert_eq!(
            reply,
            Ok(OkReply::FileQueued {
                file_id: [0xF1; 16],
                size: 254_000,
                chunks: 16,
            })
        );

        // FILE_RECEIVED.
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7);
        payload.extend_from_slice(&[0xF1; 16]);
        api::put::u32(&mut payload, 254_000);
        api::put::str16(&mut payload, "макет.pdf").expect("str16");
        let header = Header {
            kind: Kind::Event,
            id: api::Event::FileReceived as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        assert_eq!(
            decode_event(&header, &payload),
            Some(LinkEvent::FileReceived {
                cid: Cid(7),
                file_id: [0xF1; 16],
                size: 254_000,
                name: "макет.pdf".into(),
            })
        );

        // FILE_SENT.
        let header = Header {
            id: api::Event::FileSent as u16,
            ..header
        };
        assert_eq!(
            decode_event(&header, &payload),
            Some(LinkEvent::FileSent {
                cid: Cid(7),
                file_id: [0xF1; 16],
            })
        );
    }

    /// S8б: SET_AVATAR/GET_AVATAR_ID — кадры и ответы; AVATAR_RECEIVED.
    #[test]
    fn avatar_wire_roundtrip() {
        let (method, payload) = encode_request(
            &Request::SetAvatar {
                data: vec![0x89, 0x50, 0x4E, 0x47],
            },
            true,
        );
        assert_eq!(method, Method::SetAvatar);
        let mut r = api::Reader::new(&payload);
        assert_eq!(r.read_u32(), Ok(4)); // bytes32: длина
        assert_eq!(r.read_bytes(4), Ok(&[0x89, 0x50, 0x4E, 0x47][..]));

        let (method, payload) = encode_request(&Request::GetAvatarId { cid: Cid(7) }, true);
        assert_eq!(method, Method::GetAvatarId);
        assert_eq!(payload, 7u32.to_le_bytes());

        // Ответ SET_AVATAR: [avatar_id 16][size u32].
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0xA1; 16]);
        api::put::u32(&mut raw, 105);
        let reply = decode_reply(
            Method::SetAvatar,
            RawReply {
                status: Status::Ok,
                payload: raw,
            },
            true,
        );
        assert_eq!(
            reply,
            Ok(OkReply::AvatarSet {
                avatar_id: [0xA1; 16],
                size: 105,
            })
        );

        // Ответ GET_AVATAR_ID: нули — нет аватарки.
        let reply = decode_reply(
            Method::GetAvatarId,
            RawReply {
                status: Status::Ok,
                payload: [0; 16].to_vec(),
            },
            true,
        );
        assert_eq!(reply, Ok(OkReply::AvatarId([0; 16])));

        // AVATAR_RECEIVED: [cid u32][avatar_id 16][data str16].
        let mut payload = Vec::new();
        api::put::u32(&mut payload, 7);
        payload.extend_from_slice(&[0xA1; 16]);
        api::put::bytes16(&mut payload, &[1, 2, 3]).expect("bytes16");
        let header = Header {
            kind: Kind::Event,
            id: api::Event::AvatarReceived as u16,
            request_id: 0,
            status: Status::Ok,
            payload_len: payload.len() as u32,
        };
        assert_eq!(
            decode_event(&header, &payload),
            Some(LinkEvent::AvatarReceived {
                cid: Cid(7),
                avatar_id: [0xA1; 16],
                data: vec![1, 2, 3],
            })
        );
    }
}
