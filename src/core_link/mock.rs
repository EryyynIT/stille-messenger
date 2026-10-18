//! Заглушка ядра: демо-сцена, автоответы, флип presence.
//!
//! Не тестовый костыль, а равноправная реализация [`CoreLink`]
//! (`--mock`): семантика повторяет реальный канал — офлайн-отправка
//! уходит в outbox (queued), ACK приходит отдельно (доставлено).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_core::Stream;
use std::future::Future;
use tokio::sync::mpsc;
use tokio::time;

use crate::core_link::api::Status;
use crate::core_link::{
    entities_valid, CallSignal, CoreLink, Entity, EntityKind, HistoryMsg, LinkError, LinkEvent,
    OkReply, Reply, Request, SearchHit,
};
use crate::model::cid::Cid;
use crate::model::msg::CoreMsgId;
use crate::model::store::Contact;

/// msg_id из номера — детерминированные 16 байт для демо/тестов.
fn core_id(n: u64) -> CoreMsgId {
    let mut bytes = [0u8; 16];
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    CoreMsgId(bytes)
}

/// Сцена заглушки: кто в контактах и что происходит по таймеру.
#[derive(Debug, Clone)]
pub struct Scene {
    pub contacts: Vec<SceneContact>,
    /// Отложенные события от старта подписки.
    pub scripted: Vec<(Duration, LinkEvent)>,
    /// Флип presence случайного контакта каждые N (демо: 8 с).
    pub flip_presence: Option<Duration>,
    /// Задержка автоответа (демо: ~700 мс, 650–850 с джиттером).
    pub reply_delay_ms: u64,
    /// Реплики автоответа по кругу; в тестах — детерминированные.
    pub replies: Vec<String>,
    /// Что ядро «помнит» по контактам: ответ на GET_HISTORY. ts задан
    /// при конструировании сцены — для демо-сцены это «вчера».
    pub history: HashMap<i64, Vec<HistoryMsg>>,
}

#[derive(Debug, Clone)]
pub struct SceneContact {
    pub cid: i64,
    pub name: String,
    pub online: bool,
}

impl Scene {
    /// Демо-сцена: Аня с непрочитанными, Борис офлайн, Вера онлайн.
    pub fn demo() -> Self {
        Self {
            contacts: vec![
                SceneContact {
                    cid: 1,
                    name: "Аня".into(),
                    online: true,
                },
                SceneContact {
                    cid: 2,
                    name: "Борис".into(),
                    online: false,
                },
                SceneContact {
                    cid: 3,
                    name: "Вера".into(),
                    online: true,
                },
            ],
            scripted: vec![
                (
                    Duration::from_millis(150),
                    LinkEvent::MessageReceived {
                        cid: Cid(1),
                        name: "Аня".into(),
                        msg_id: core_id(1001),
                        ts_ms: crate::core_link::now_ms(),
                        text: "привет!".into(),
                        entities: Vec::new(),
                    },
                ),
                (
                    Duration::from_millis(250),
                    LinkEvent::MessageReceived {
                        cid: Cid(1),
                        name: "Аня".into(),
                        msg_id: core_id(1002),
                        ts_ms: crate::core_link::now_ms(),
                        text: "как дела?".into(),
                        entities: Vec::new(),
                    },
                ),
                (
                    Duration::from_millis(350),
                    LinkEvent::MessageReceived {
                        cid: Cid(2),
                        name: "Борис".into(),
                        msg_id: core_id(1003),
                        ts_ms: crate::core_link::now_ms(),
                        text: "есть минута?".into(),
                        entities: Vec::new(),
                    },
                ),
                (
                    // Жё и Шё в кадре — проверка кириллицы жирных глифов
                    // на скриншоте (GUI_DEV.md §3).
                    Duration::from_millis(450),
                    LinkEvent::MessageReceived {
                        cid: Cid(1),
                        name: "Аня".into(),
                        msg_id: core_id(1004),
                        ts_ms: crate::core_link::now_ms(),
                        text: "Жёсткий дедлайн. Шёпотом: переждём.".into(),
                        // Демо-разметка (S14): жирный + код + спойлер —
                        // скриншот демо-сцены показывает рендер spans.
                        // offsets — байты UTF-8: «Жёсткий» 0..14,
                        // «Шёпотом» 31..45, «переждём.» 47..64.
                        entities: vec![
                            Entity::new(EntityKind::Bold, 0, 14),
                            Entity::new(EntityKind::Code, 31, 14),
                            Entity::new(EntityKind::Spoiler, 47, 17),
                        ],
                    },
                ),
                // Входящий файл (S10): «макет.pdf» — пузырь с кнопкой
                // открытия на скриншоте демо.
                (
                    Duration::from_millis(700),
                    LinkEvent::FileReceived {
                        cid: Cid(1),
                        file_id: [0xF1; 16],
                        size: 254_000,
                        name: "макет.pdf".into(),
                    },
                ),
                // Аватарка Ани (S8б): ядро «дотащило» байты у пира —
                // кружок в сайдбаре и шапке чата на скриншоте демо.
                (
                    Duration::from_millis(900),
                    LinkEvent::AvatarReceived {
                        cid: Cid(1),
                        avatar_id: [0xA1; 16],
                        data: include_bytes!("demo_avatar.png").to_vec(),
                    },
                ),
                // Реакция собеседника (S15): 👍 на размеченное сообщение —
                // скриншот демо показывает пилюлю под пузырём.
                (
                    Duration::from_millis(600),
                    LinkEvent::Reaction {
                        cid: Cid(1),
                        msg_id: core_id(1004),
                        add: true,
                        emoji: '👍',
                        ts_ms: crate::core_link::now_ms(),
                    },
                ),
            ],
            flip_presence: Some(Duration::from_secs(8)),
            reply_delay_ms: 700,
            replies: demo_replies(),
            history: demo_history(),
        }
    }

    /// Детерминированная сцена для UI-тестов: один онлайн-контакт,
    /// без флипа presence, быстрый автоответ.
    pub fn smoke() -> Self {
        Self {
            contacts: vec![SceneContact {
                cid: 1,
                name: "Аня".into(),
                online: true,
            }],
            scripted: vec![(
                Duration::from_millis(300),
                LinkEvent::MessageReceived {
                    cid: Cid(1),
                    name: "Аня".into(),
                    msg_id: core_id(2001),
                    ts_ms: crate::core_link::now_ms(),
                    text: "пинг".into(),
                    entities: Vec::new(),
                },
            )],
            flip_presence: None,
            reply_delay_ms: 100,
            replies: vec!["принято".into()],
            history: HashMap::new(),
        }
    }
}

/// Вчерашняя переписка для демо-сцены: у Ани — диалог с доставкой,
/// у Бориса — одно своё офлайн-сообщение без ACK.
fn demo_history() -> HashMap<i64, Vec<HistoryMsg>> {
    let day = 24 * 60 * 60 * 1000;
    let yesterday = crate::core_link::now_ms() - day;
    HashMap::from([
        (
            1,
            vec![
                HistoryMsg {
                    mine: false,
                    delivered: false,
                    msg_id: core_id(900),
                    ts_ms: yesterday,
                    text: "слушай, ты вчера был на созвоне?".into(),
                    entities: Vec::new(),
                },
                HistoryMsg {
                    mine: true,
                    delivered: true,
                    msg_id: core_id(901),
                    ts_ms: yesterday + 60_000,
                    text: "был. договорились о сроках".into(),
                    entities: Vec::new(),
                },
                HistoryMsg {
                    mine: false,
                    delivered: false,
                    msg_id: core_id(902),
                    ts_ms: yesterday + 120_000,
                    text: "отлично, тогда жду макет".into(),
                    entities: Vec::new(),
                },
            ],
        ),
        (
            2,
            vec![HistoryMsg {
                mine: true,
                delivered: false,
                msg_id: core_id(910),
                ts_ms: yesterday + 3_600_000,
                text: "напиши, как доберёшься".into(),
                entities: Vec::new(),
            }],
        ),
    ])
}

/// Заглушка [`crate::core_link::CoreLink`].
pub struct MockLink {
    inner: Arc<Inner>,
    scene: Arc<Scene>,
}

struct Inner {
    state: Mutex<State>,
    tx: mpsc::Sender<LinkEvent>,
    rx: Mutex<Option<mpsc::Receiver<LinkEvent>>>,
    msg_seq: AtomicU64,
    /// Очередь офлайн-отправок по контактам: уйдёт «доставлено» при флипе.
    queued: Mutex<HashMap<Cid, Vec<CoreMsgId>>>,
}

struct State {
    contacts: Vec<MockContact>,
    lcg: Lcg,
    guest_seq: u64,
    /// Закреплённый чат (семантика 2.2: один на демон, идемпотентно).
    pinned_chat: Option<Cid>,
    /// Закреплённые сообщения по чатам (≤16, порядок закрепления).
    pinned_msgs: HashMap<Cid, Vec<CoreMsgId>>,
    /// Текущий звонок (§4.6: одна линия на демон).
    call: Option<MockCall>,
    /// avatar_id по cid (0 — своя); байты отдаются событием (S8б).
    avatar_ids: HashMap<u32, [u8; 16]>,
}

struct MockCall {
    cid: Cid,
    call_id: [u8; 16],
    /// true — разговор идёт (accept прошёл).
    up: bool,
}

struct MockContact {
    cid: Cid,
    name: String,
    online: bool,
    blocked: bool,
}

/// Детерминированный «шум» без внешнего крейта: джиттер задержки и
/// выбор реплики автоответа.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

const REPLIES: [&str; 5] = ["ок", "принято", "понял", "согласен", "давай"];

fn demo_replies() -> Vec<String> {
    REPLIES.into_iter().map(str::to_owned).collect()
}

impl MockLink {
    pub fn new(scene: Scene) -> Self {
        let (tx, rx) = mpsc::channel(128);
        let contacts = scene
            .contacts
            .iter()
            .map(|c| MockContact {
                cid: Cid(c.cid),
                name: c.name.clone(),
                online: c.online,
                blocked: false,
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    contacts,
                    lcg: Lcg(0x5e0f),
                    guest_seq: 0,
                    pinned_chat: None,
                    pinned_msgs: HashMap::new(),
                    call: None,
                    avatar_ids: HashMap::new(),
                }),
                tx,
                rx: Mutex::new(Some(rx)),
                msg_seq: AtomicU64::new(1),
                queued: Mutex::new(HashMap::new()),
            }),
            scene: Arc::new(scene),
        }
    }
}

impl Hash for MockLink {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        Arc::as_ptr(&self.inner).hash(hasher);
    }
}

impl CoreLink for MockLink {
    fn events(self: Arc<Self>) -> impl Stream<Item = LinkEvent> + Send + 'static {
        let Some(rx) = self.inner.rx.lock().expect("rx не занят").take() else {
            panic!("подписка на события заглушки уже взята");
        };

        // Сценарные события.
        for (delay, event) in self.scene.scripted.clone() {
            let tx = self.inner.tx.clone();
            tokio::spawn(async move {
                time::sleep(delay).await;
                let _ = tx.send(event).await;
            });
        }

        // Флип presence случайного контакта.
        if let Some(period) = self.scene.flip_presence {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                let mut ticker = time::interval(period);
                ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
                ticker.tick().await; // первый тик мгновенный — пропускаем
                loop {
                    ticker.tick().await;
                    for event in flip_random(&inner) {
                        let _ = inner.tx.send(event).await;
                    }
                }
            });
        }

        super::RecvStream(rx)
    }

    fn execute(&self, request: Request) -> impl Future<Output = Reply> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        let scene = Arc::clone(&self.scene);
        async move { Self::handle(&inner, request, &scene).await }
    }

    fn is_stub(&self) -> bool {
        true
    }
}

impl MockLink {
    async fn handle(inner: &Arc<Inner>, request: Request, scene: &Scene) -> Reply {
        let reply_delay_ms = scene.reply_delay_ms;
        let scene_replies = &scene.replies;
        match request {
            Request::Contacts => {
                let state = inner.state.lock().expect("стейт не отравлен");
                let contacts = state
                    .contacts
                    .iter()
                    .map(|c| Contact {
                        cid: c.cid,
                        name: c.name.clone(),
                        online: c.online,
                        blocked: c.blocked,
                        avatar_id: None,
                    })
                    .collect();
                Ok(OkReply::Contacts(contacts))
            }
            Request::SendMsg {
                cid,
                text,
                entities,
            } => {
                // Семантика ядра: кривая разметка — BAD_ARGS до отправки.
                if entities_valid(&text, &entities).is_err() {
                    return Err(LinkError::Kernel(Status::BadArgs));
                }
                let (online, name) = {
                    let state = inner.state.lock().expect("стейт не отравлен");
                    match state.contacts.iter().find(|c| c.cid == cid) {
                        None => return Err(LinkError::Kernel(Status::NoContact)),
                        Some(c) if c.blocked => {
                            return Err(LinkError::Kernel(Status::ContactBlocked));
                        }
                        Some(c) => (c.online, c.name.clone()),
                    }
                };

                let msg_id = core_id(inner.msg_seq.fetch_add(1, Ordering::Relaxed));
                let tx = inner.tx.clone();

                if !online {
                    // Семантика ядра: офлайн — не ошибка, а outbox.
                    inner
                        .queued
                        .lock()
                        .expect("queue не отравлен")
                        .entry(cid)
                        .or_default()
                        .push(msg_id);
                    return Ok(OkReply::Sent {
                        msg_id,
                        queued: true,
                    });
                }

                let (delay, reply_text) = {
                    let mut state = inner.state.lock().expect("стейт не отравлен");
                    let jitter = state.lcg.next() % 200;
                    let reply =
                        scene_replies[(state.lcg.next() as usize) % scene_replies.len()].clone();
                    (
                        Duration::from_millis(
                            reply_delay_ms.saturating_add(jitter).saturating_sub(100),
                        ),
                        reply,
                    )
                };
                tokio::spawn(async move {
                    // ACK от пира, затем автоответ — как живой цикл доставки.
                    time::sleep(delay / 4).await;
                    let _ = tx.send(LinkEvent::MessageDelivered { cid, msg_id }).await;
                    time::sleep(delay).await;
                    let _ = tx
                        .send(LinkEvent::MessageReceived {
                            cid,
                            name,
                            msg_id,
                            ts_ms: crate::core_link::now_ms(),
                            text: reply_text,
                            entities: Vec::new(),
                        })
                        .await;
                });
                Ok(OkReply::Sent {
                    msg_id,
                    queued: false,
                })
            }
            Request::Invite { relay, addr } => {
                let mut state = inner.state.lock().expect("стейт не отравлен");
                let code: String = (0..80)
                    .map(|_| B64URL[(state.lcg.next() % 64) as usize] as char)
                    .collect();
                let host = if addr.is_empty() {
                    "192.168.1.5".to_owned()
                } else {
                    addr
                };
                let head = if relay {
                    "stille://i/r/"
                } else {
                    "stille://i/"
                };
                Ok(OkReply::Code(format!("{head}{code}@{host}")))
            }
            // Семантика ядра (§4.4): 1..=16000 Б, id — content
            // addressing (заглушка — синтетический, как msg_id).
            Request::SetAvatar { data } => {
                // Ядро отвечает одним кодом на обе рамки размера
                // (проверено e2e): пустая и >16000 — TooBig.
                if data.is_empty() || data.len() > 16000 {
                    return Err(LinkError::Kernel(Status::TooBig));
                }
                let avatar_id = core_id(0xA000 + inner.msg_seq.fetch_add(1, Ordering::Relaxed)).0;
                inner
                    .state
                    .lock()
                    .expect("стейт не отравлен")
                    .avatar_ids
                    .insert(0, avatar_id); // своя
                Ok(OkReply::AvatarSet {
                    avatar_id,
                    size: data.len() as u32,
                })
            }
            // GET_AVATAR_ID: известный id или нули (нет аватарки).
            Request::GetAvatarId { cid } => {
                let state = inner.state.lock().expect("стейт не отравлен");
                let id = state
                    .avatar_ids
                    .get(&(cid.0 as u32))
                    .copied()
                    .unwrap_or([0; 16]);
                Ok(OkReply::AvatarId(id))
            }
            // Семантика ядра (§4.5): путь читается с диска, лимит 64 МБ,
            // basename ≤ 64 байт; контакт онлайн — через паузу событие
            // FILE_SENT (честный «отправлен»), офлайн — тихий outbox.
            Request::SendFile { cid, path } => {
                let known = {
                    let state = inner.state.lock().expect("стейт не отравлен");
                    state
                        .contacts
                        .iter()
                        .find(|c| c.cid == cid)
                        .map(|c| c.online)
                };
                let Some(online) = known else {
                    return Err(LinkError::Kernel(Status::NoContact));
                };
                let name = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.is_empty() || name.len() > 64 {
                    return Err(LinkError::Kernel(Status::BadArgs));
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    return Err(LinkError::Kernel(Status::FileNotFound));
                };
                let size = meta.len();
                if size > 64 * 1024 * 1024 {
                    return Err(LinkError::Kernel(Status::TooBig));
                }
                let file_id = core_id(inner.msg_seq.fetch_add(1, Ordering::Relaxed)).0;
                if online {
                    let tx = inner.tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(reply_delay_ms)).await;
                        let _ = tx.send(LinkEvent::FileSent { cid, file_id }).await;
                    });
                }
                Ok(OkReply::FileQueued {
                    file_id,
                    size: size as u32,
                    chunks: (size.div_ceil(16000)) as u32,
                })
            }
            // Семантика ядра: реакция требует известного контакта; msg_id
            // ядро не проверяет в контракте — заглушка тоже (Done).
            Request::SetReaction { cid, .. } | Request::RemoveReaction { cid, .. } => {
                let known = {
                    let state = inner.state.lock().expect("стейт не отравлен");
                    state.contacts.iter().any(|c| c.cid == cid)
                };
                if !known {
                    return Err(LinkError::Kernel(Status::NoContact));
                }
                Ok(OkReply::Done)
            }
            Request::Add { code } => {
                if !code.starts_with("stille://i/")
                    && !code.starts_with("shopot1.")
                    && !code.starts_with("shopot://i/")
                {
                    return Err(LinkError::Kernel(Status::BadInviteCode));
                }
                let (cid, guest_name) = {
                    let mut state = inner.state.lock().expect("стейт не отравлен");
                    state.guest_seq += 1;
                    let guest = state.guest_seq;
                    let cid = Cid(100 + guest as i64);
                    let guest_name = format!("Гость {guest}");
                    state.contacts.push(MockContact {
                        cid,
                        name: guest_name.clone(),
                        online: false,
                        blocked: false,
                    });
                    (cid, guest_name)
                };
                let tx = inner.tx.clone();
                tokio::spawn(async move {
                    time::sleep(Duration::from_millis(400)).await;
                    let _ = tx
                        .send(LinkEvent::ContactJoined {
                            cid,
                            name: guest_name,
                        })
                        .await;
                });
                Ok(OkReply::Added { cid })
            }
            Request::Block { cid } | Request::Unblock { cid } => {
                let blocked = matches!(request, Request::Block { .. });
                let mut state = inner.state.lock().expect("стейт не отравлен");
                match state.contacts.iter_mut().find(|c| c.cid == cid) {
                    Some(contact) => contact.blocked = blocked,
                    None => return Err(LinkError::Kernel(Status::NoContact)),
                }
                Ok(OkReply::Done)
            }
            Request::Version => Ok(OkReply::Version {
                major: 2,
                minor: 2,
                patch: 0,
            }),
            Request::History { cid, limit } => {
                let known = {
                    let state = inner.state.lock().expect("стейт не отравлен");
                    state.contacts.iter().any(|c| c.cid == cid)
                };
                if !known {
                    return Err(LinkError::Kernel(Status::NoContact));
                }
                // Семантика ядра: окно новейших записей, старые → новые.
                let all = scene.history.get(&cid.0);
                let total = all.map_or(0, |h| h.len() as u32);
                let skip = all.map_or(0, |h| h.len().saturating_sub(limit as usize));
                let records = all.map_or_else(Vec::new, |h| h[skip..].to_vec());
                Ok(OkReply::History { total, records })
            }
            Request::Search {
                cid,
                query,
                limit,
                offset,
            } => {
                // Заглушка ищет по сценарной истории подстрокой без учёта
                // регистра — тот же путь UI, что у ядерного поиска (2.1+),
                // но без нечётких совпадений (fuzzy всегда false).
                let needle = query.trim().to_lowercase();
                if needle.is_empty() {
                    return Err(LinkError::Kernel(Status::BadArgs));
                }
                let limit = if limit == 0 { 50 } else { limit.min(200) };
                let mut matched: Vec<SearchHit> = Vec::new();
                let mut collect = |cid: Cid, records: &[HistoryMsg]| {
                    // Ядро отдаёт точные совпадения новые → старые.
                    for record in records.iter().rev() {
                        if record.text.to_lowercase().contains(&needle) {
                            matched.push(SearchHit {
                                cid,
                                mine: record.mine,
                                delivered: record.delivered,
                                fuzzy: false,
                                msg_id: record.msg_id,
                                ts_ms: record.ts_ms,
                                body: record.text.clone(),
                                entities: record.entities.clone(),
                            });
                        }
                    }
                };
                match cid {
                    Some(cid) => {
                        if let Some(records) = scene.history.get(&cid.0) {
                            collect(cid, records);
                        }
                    }
                    None => {
                        for (cid, records) in &scene.history {
                            collect(Cid(*cid), records);
                        }
                    }
                }
                let total = matched.len() as u32;
                let page = matched
                    .into_iter()
                    .skip(offset as usize)
                    .take(limit as usize)
                    .collect();
                Ok(OkReply::Search { total, hits: page })
            }
            Request::PinChat { cid } => {
                let mut state = inner.state.lock().expect("стейт не отравлен");
                if !state.contacts.iter().any(|c| c.cid == cid) {
                    return Err(LinkError::Kernel(Status::NoContact));
                }
                state.pinned_chat = Some(cid);
                Ok(OkReply::Done)
            }
            Request::UnpinChat => {
                inner.state.lock().expect("стейт не отравлен").pinned_chat = None;
                Ok(OkReply::Done)
            }
            Request::PinnedChat => {
                let cid = inner.state.lock().expect("стейт не отравлен").pinned_chat;
                Ok(OkReply::PinnedChat { cid })
            }
            Request::PinnedMessages { cid } => {
                let state = inner.state.lock().expect("стейт не отравлен");
                let ids = state.pinned_msgs.get(&cid).cloned().unwrap_or_default();
                Ok(OkReply::PinnedMessages { ids })
            }
            Request::PinMessage { cid, msg_id } => {
                // Семантика ядра: сообщение обязано быть в истории чата,
                // лимит — 16 закрепов на чат (§4.8).
                let known = scene
                    .history
                    .get(&cid.0)
                    .is_some_and(|recs| recs.iter().any(|r| r.msg_id == msg_id));
                if !known {
                    return Err(LinkError::Kernel(Status::NotFound));
                }
                let mut state = inner.state.lock().expect("стейт не отравлен");
                let pins = state.pinned_msgs.entry(cid).or_default();
                if pins.contains(&msg_id) {
                    return Ok(OkReply::Done);
                }
                if pins.len() >= 16 {
                    return Err(LinkError::Kernel(Status::TooBig));
                }
                pins.push(msg_id);
                Ok(OkReply::Done)
            }
            Request::UnpinMessage { cid, msg_id } => {
                let mut state = inner.state.lock().expect("стейт не отравлен");
                if let Some(pins) = state.pinned_msgs.get_mut(&cid) {
                    pins.retain(|id| *id != msg_id);
                }
                Ok(OkReply::Done)
            }
            Request::GetMessage { msg_id } => {
                // Заглушка знает записи сценарной истории.
                for (cid, records) in &scene.history {
                    if let Some(record) = records.iter().find(|r| r.msg_id == msg_id) {
                        return Ok(OkReply::Message {
                            cid: Cid(*cid),
                            mine: record.mine,
                            delivered: record.delivered,
                            ts_ms: record.ts_ms,
                            text: record.text.clone(),
                            entities: record.entities.clone(),
                        });
                    }
                }
                Err(LinkError::Kernel(Status::NotFound))
            }
            Request::DeleteChat { cid } => {
                // Семантика ядра: контакт исчезает из списка, закрепы
                // и очередь офлайн-отправок чата чистятся.
                let mut state = inner.state.lock().expect("стейт не отравлен");
                let Some(pos) = state.contacts.iter().position(|c| c.cid == cid) else {
                    return Err(LinkError::Kernel(Status::NoContact));
                };
                state.contacts.remove(pos);
                if state.pinned_chat == Some(cid) {
                    state.pinned_chat = None;
                }
                state.pinned_msgs.remove(&cid);
                inner.queued.lock().expect("queue не отравлен").remove(&cid);
                Ok(OkReply::Done)
            }
            Request::SendContact { to, contact } => {
                // Семантика ядра: карточка уходит только по живой сессии;
                // contact = None — моя карточка, всегда существует.
                let state = inner.state.lock().expect("стейт не отравлен");
                let Some(target) = state.contacts.iter().find(|c| c.cid == to) else {
                    return Err(LinkError::Kernel(Status::NoContact));
                };
                if target.blocked {
                    return Err(LinkError::Kernel(Status::ContactBlocked));
                }
                if let Some(card) = contact {
                    if !state.contacts.iter().any(|c| c.cid == card) {
                        return Err(LinkError::Kernel(Status::NoContact));
                    }
                }
                if !target.online {
                    return Err(LinkError::Kernel(Status::NoSession));
                }
                Ok(OkReply::Done)
            }
            Request::SetName { name } => {
                // Контракт §4.4: 1..=32 символа; ядро отвечает BAD_ARGS.
                let len = name.chars().count();
                if !(1..=32).contains(&len) {
                    return Err(LinkError::Kernel(Status::BadArgs));
                }
                Ok(OkReply::Done)
            }
            Request::StartCall { cid } => {
                // Семантика ядра (§4.6): одна линия — BUSY; звонок только
                // по живому соединению (NO_SESSION). Ядро шлёт локальный
                // offer сразу; онлайн-пир «принимает» через задержку
                // автоответа сцены.
                let call_id = core_id(inner.msg_seq.fetch_add(1, Ordering::Relaxed)).0;
                {
                    // Guard мьютекса не Send: вся работа со стейтом — в
                    // своём скоупе, до первого await.
                    let mut state = inner.state.lock().expect("стейт не отравлен");
                    if state.call.is_some() {
                        return Err(LinkError::Kernel(Status::Busy));
                    }
                    let (blocked, online) = match state.contacts.iter().find(|c| c.cid == cid) {
                        None => return Err(LinkError::Kernel(Status::NoContact)),
                        Some(contact) => (contact.blocked, contact.online),
                    };
                    if blocked {
                        return Err(LinkError::Kernel(Status::ContactBlocked));
                    }
                    if !online {
                        return Err(LinkError::Kernel(Status::NoSession));
                    }
                    state.call = Some(MockCall {
                        cid,
                        call_id,
                        up: false,
                    });
                }
                let _ = inner
                    .tx
                    .send(LinkEvent::CallState {
                        cid,
                        call_id,
                        state: CallSignal::Offer,
                    })
                    .await;
                let inner2 = Arc::clone(inner);
                let delay = Duration::from_millis(scene.reply_delay_ms);
                tokio::spawn(async move {
                    time::sleep(delay).await;
                    let up = {
                        let mut state = inner2.state.lock().expect("стейт не отравлен");
                        match &mut state.call {
                            Some(call) if call.call_id == call_id => {
                                call.up = true;
                                true
                            }
                            // Звонок уже завершён/сменился — accept не шлём.
                            _ => false,
                        }
                    };
                    if !up {
                        return;
                    }
                    let _ = inner2
                        .tx
                        .send(LinkEvent::CallState {
                            cid,
                            call_id,
                            state: CallSignal::Accept,
                        })
                        .await;
                });
                Ok(OkReply::CallStarted { call_id })
            }
            Request::AnswerCall => {
                // Семантика ядра: без звонка — NO_CALL, повтор — OK
                // (идемпотентно); accept — локальное событие.
                let target = {
                    let mut state = inner.state.lock().expect("стейт не отравлен");
                    match &mut state.call {
                        None => return Err(LinkError::Kernel(Status::NoCall)),
                        Some(call) if call.up => None, // уже в разговоре
                        Some(call) => {
                            call.up = true;
                            Some((call.cid, call.call_id))
                        }
                    }
                };
                let Some((cid, call_id)) = target else {
                    return Ok(OkReply::Done);
                };
                let _ = inner
                    .tx
                    .send(LinkEvent::CallState {
                        cid,
                        call_id,
                        state: CallSignal::Accept,
                    })
                    .await;
                Ok(OkReply::Done)
            }
            Request::EndCall => {
                // Семантика ядра: без звонка — NO_CALL; завершение —
                // локальное событие end (пиру ядро шлёт hangup).
                let call = inner.state.lock().expect("стейт не отравлен").call.take();
                let Some(call) = call else {
                    return Err(LinkError::Kernel(Status::NoCall));
                };
                let _ = inner
                    .tx
                    .send(LinkEvent::CallState {
                        cid: call.cid,
                        call_id: call.call_id,
                        state: CallSignal::End,
                    })
                    .await;
                Ok(OkReply::Done)
            }
        }
    }
}

fn flip_random(inner: &Arc<Inner>) -> Vec<LinkEvent> {
    let index = {
        let mut state = inner.state.lock().expect("стейт не отравлен");
        if state.contacts.is_empty() {
            return Vec::new();
        }
        (state.lcg.next() as usize) % state.contacts.len()
    };
    flip_contact(inner, index)
}

/// Флип presence контакта по индексу:Presence + квази-ACK очереди при
/// выходе в онлайн. Индекс наружу — для детерминированных тестов.
fn flip_contact(inner: &Arc<Inner>, index: usize) -> Vec<LinkEvent> {
    let mut state = inner.state.lock().expect("стейт не отравлен");
    let Some(contact) = state.contacts.get_mut(index) else {
        return Vec::new();
    };
    contact.online = !contact.online;
    let online = contact.online;
    let cid = contact.cid;
    let mut events = vec![LinkEvent::Presence {
        cid,
        online,
        last_seen_ms: 0,
    }];
    if online {
        // Квази-ACK: outbox контакта доехал.
        let delivered = inner
            .queued
            .lock()
            .expect("queue не отравлен")
            .remove(&cid)
            .unwrap_or_default();
        for msg_id in delivered {
            events.push(LinkEvent::MessageDelivered { cid, msg_id });
        }
    }
    events
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Хелпер тестов: собрать первые `n` событий из потока.
#[cfg(test)]
pub(crate) use super::take_events;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_link::CoreLink;

    fn scene() -> Scene {
        Scene {
            contacts: vec![
                SceneContact {
                    cid: 1,
                    name: "Аня".into(),
                    online: true,
                },
                SceneContact {
                    cid: 2,
                    name: "Борис".into(),
                    online: false,
                },
            ],
            scripted: vec![],
            flip_presence: None,
            reply_delay_ms: 50,
            replies: demo_replies(),
            history: HashMap::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn contacts_are_typed() {
        let link = MockLink::new(scene());
        let reply = link.execute(Request::Contacts).await;
        let Ok(OkReply::Contacts(contacts)) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].cid, Cid(1));
        assert_eq!(contacts[0].name, "Аня");
        assert!(contacts[0].online);
        assert!(!contacts[1].blocked);
    }

    #[tokio::test(start_paused = true)]
    async fn send_to_online_contact_delivers_and_autoreplies() {
        let link = Arc::new(MockLink::new(scene()));

        let reply = link
            .execute(Request::SendMsg {
                cid: Cid(1),
                text: "привет".into(),
                entities: Vec::new(),
            })
            .await;
        let Ok(OkReply::Sent { queued, .. }) = reply else {
            panic!("{reply:?}");
        };
        assert!(!queued, "онлайн-контакт получает сразу");

        let mut events = take_events(CoreLink::events(Arc::clone(&link)), 2).await;
        assert!(
            matches!(
                events.pop_front(),
                Some(LinkEvent::MessageDelivered { cid: Cid(1), .. })
            ),
            "сначала ACK доставки"
        );
        let event = events.pop_front().expect("автоответ приходит");
        let LinkEvent::MessageReceived { cid, text, .. } = event else {
            panic!("ожидали MessageReceived, получено {event:?}");
        };
        assert_eq!(cid, Cid(1));
        assert!(!text.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn send_to_offline_contact_is_queued_until_flip() {
        let link = Arc::new(MockLink::new(scene()));

        let reply = link
            .execute(Request::SendMsg {
                cid: Cid(2),
                text: "привет".into(),
                entities: Vec::new(),
            })
            .await;
        let Ok(OkReply::Sent { queued, .. }) = reply else {
            panic!("{reply:?}");
        };
        assert!(queued, "офлайн — это outbox, не ошибка");

        // Борис выходит в онлайн — квази-ACK: очередь доехала.
        for event in flip_contact(&link.inner, 1) {
            let _ = link.inner.tx.send(event).await;
        }
        let mut events = take_events(CoreLink::events(Arc::clone(&link)), 2).await;
        assert!(matches!(
            events.pop_front(),
            Some(LinkEvent::Presence { online: true, .. })
        ));
        assert!(matches!(
            events.pop_front(),
            Some(LinkEvent::MessageDelivered { cid: Cid(2), .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn add_valid_code_creates_contact_and_announces_join() {
        let link = Arc::new(MockLink::new(scene()));

        let reply = link
            .execute(Request::Add {
                code: "stille://i/abc@1.2.3.4".into(),
            })
            .await;
        let Ok(OkReply::Added { cid }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(cid, Cid(101));

        let mut events = take_events(CoreLink::events(Arc::clone(&link)), 1).await;
        let event = events.pop_front().expect("ContactJoined приходит");
        assert_eq!(
            event,
            LinkEvent::ContactJoined {
                cid: Cid(101),
                name: "Гость 1".into()
            }
        );

        let Ok(OkReply::Contacts(contacts)) = link.execute(Request::Contacts).await else {
            panic!();
        };
        assert!(contacts.iter().any(|c| c.name == "Гость 1"));
    }

    #[tokio::test(start_paused = true)]
    async fn add_bad_code_is_kernel_error() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::Add {
                code: "mqtt://broker".into(),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::BadInviteCode)));
        assert!(reply.unwrap_err().user_text().contains("Скопируйте"));
    }

    #[tokio::test(start_paused = true)]
    async fn invite_code_has_v2_shape() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::Invite {
                relay: false,
                addr: String::new(),
            })
            .await;
        let Ok(OkReply::Code(code)) = reply else {
            panic!()
        };
        assert!(code.starts_with("stille://i/"));
        assert!(code.ends_with("@192.168.1.5"));
    }

    #[tokio::test(start_paused = true)]
    async fn block_marks_contact() {
        let link = MockLink::new(scene());
        assert_eq!(
            link.execute(Request::Block { cid: Cid(1) }).await,
            Ok(OkReply::Done)
        );
        let Ok(OkReply::Contacts(contacts)) = link.execute(Request::Contacts).await else {
            panic!();
        };
        assert!(contacts[0].blocked);
        assert_eq!(
            link.execute(Request::Unblock { cid: Cid(1) }).await,
            Ok(OkReply::Done)
        );
        let Ok(OkReply::Contacts(contacts)) = link.execute(Request::Contacts).await else {
            panic!();
        };
        assert!(!contacts[0].blocked);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_contact_send_is_kernel_no_contact() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::SendMsg {
                cid: Cid(99),
                text: "эй".into(),
                entities: Vec::new(),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));
        assert_eq!(reply.unwrap_err().user_text(), "Контакт не найден.");
    }

    /// Окно GET_HISTORY: новейшие `limit` записей, порядок старые → новые,
    /// total не зависит от окна.
    #[tokio::test(start_paused = true)]
    async fn history_returns_newest_window_in_order() {
        let mut scene = scene();
        scene.history.insert(
            1,
            (0..5)
                .map(|n| HistoryMsg {
                    mine: false,
                    delivered: false,
                    msg_id: core_id(n),
                    ts_ms: n as i64 * 1000,
                    text: format!("строка {n}"),
                    entities: Vec::new(),
                })
                .collect(),
        );
        let link = MockLink::new(scene);

        let reply = link
            .execute(Request::History {
                cid: Cid(1),
                limit: 2,
            })
            .await;
        assert_eq!(
            reply,
            Ok(OkReply::History {
                total: 5,
                records: vec![record(3, "строка 3"), record(4, "строка 4"),],
            }),
            "окно = две новейшие, порядок не перевёрнут"
        );
        // Лимит больше размера — отдаётся всё.
        let reply = link
            .execute(Request::History {
                cid: Cid(1),
                limit: 50,
            })
            .await;
        assert!(matches!(
            reply,
            Ok(OkReply::History { total: 5, records }) if records.len() == 5
        ));
        // Пустая история — валидный пустой ответ.
        let reply = link
            .execute(Request::History {
                cid: Cid(2),
                limit: 50,
            })
            .await;
        assert_eq!(
            reply,
            Ok(OkReply::History {
                total: 0,
                records: vec![]
            })
        );
    }

    fn record(n: u64, text: &str) -> HistoryMsg {
        HistoryMsg {
            mine: false,
            delivered: false,
            msg_id: core_id(n),
            ts_ms: n as i64 * 1000,
            text: text.to_owned(),
            entities: Vec::new(),
        }
    }

    /// Поиск заглушки (S11): подстрока без регистра, новые первыми,
    /// limit/offset как у ядра, пустой запрос — BAD_ARGS.
    #[tokio::test(start_paused = true)]
    async fn search_matches_kernel_semantics() {
        let mut scene = scene();
        scene.history.insert(
            1,
            vec![
                record(1, "привет, как дела"),
                record(2, "второе приветствие"),
                record(3, "ПРИВЕТ ещё раз"),
            ],
        );
        scene.history.insert(2, vec![record(4, "и тут привет")]);
        let link = MockLink::new(scene);

        // Чатовый: все три записи Ани, новые первыми.
        let reply = link
            .execute(Request::Search {
                cid: Some(Cid(1)),
                query: "ПРИВЕТ".into(),
                limit: 0,
                offset: 0,
            })
            .await;
        let Ok(OkReply::Search { total, hits }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(total, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].body, "ПРИВЕТ ещё раз");
        assert!(hits.iter().all(|h| h.cid == Cid(1) && !h.fuzzy));

        // Постранично: limit/offset режут тот же список.
        let reply = link
            .execute(Request::Search {
                cid: Some(Cid(1)),
                query: "привет".into(),
                limit: 2,
                offset: 2,
            })
            .await;
        let Ok(OkReply::Search { total, hits }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(total, 3);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].body, "привет, как дела");

        // Глобальный: оба чата, cid у записей разный.
        let reply = link
            .execute(Request::Search {
                cid: None,
                query: "привет".into(),
                limit: 0,
                offset: 0,
            })
            .await;
        let Ok(OkReply::Search { total, hits }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(total, 4);
        assert!(hits.iter().any(|h| h.cid == Cid(2)));

        // Пустой запрос — BAD_ARGS, как у ядра (§4.3).
        let reply = link
            .execute(Request::Search {
                cid: Some(Cid(1)),
                query: "   ".into(),
                limit: 0,
                offset: 0,
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::BadArgs)));
    }

    /// Закрепы заглушки (S12): семантика §4.8 — чат один, закрепление
    /// идемпотентно, неизвестное сообщение — NOT_FOUND.
    #[tokio::test(start_paused = true)]
    async fn pins_match_kernel_semantics() {
        let mut scene = scene();
        scene
            .history
            .insert(1, vec![record(1, "важное"), record(2, "обычное")]);
        let link = MockLink::new(scene);

        // Чат: закреп → перезакреп на другой → чужой контакт.
        let reply = link.execute(Request::PinChat { cid: Cid(1) }).await;
        assert_eq!(reply, Ok(OkReply::Done));
        let reply = link.execute(Request::PinnedChat).await;
        assert_eq!(reply, Ok(OkReply::PinnedChat { cid: Some(Cid(1)) }));
        let _ = link.execute(Request::PinChat { cid: Cid(2) }).await;
        let reply = link.execute(Request::PinnedChat).await;
        assert_eq!(
            reply,
            Ok(OkReply::PinnedChat { cid: Some(Cid(2)) }),
            "закреп один на демон"
        );
        let reply = link.execute(Request::PinChat { cid: Cid(99) }).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));

        // Сообщения: порядок закрепления, идемпотентность, NOT_FOUND.
        let first = core_id(1);
        let second = core_id(2);
        for msg_id in [first, second, first] {
            let reply = link
                .execute(Request::PinMessage {
                    cid: Cid(1),
                    msg_id,
                })
                .await;
            assert_eq!(reply, Ok(OkReply::Done));
        }
        let reply = link.execute(Request::PinnedMessages { cid: Cid(1) }).await;
        assert_eq!(
            reply,
            Ok(OkReply::PinnedMessages {
                ids: vec![first, second]
            }),
            "повтор не дублирует"
        );
        let reply = link
            .execute(Request::PinMessage {
                cid: Cid(1),
                msg_id: core_id(77),
            })
            .await;
        assert_eq!(
            reply,
            Err(LinkError::Kernel(Status::NotFound)),
            "сообщение должно быть в истории"
        );

        // GetMessage достаёт запись; откреп идемпотентен.
        let reply = link.execute(Request::GetMessage { msg_id: first }).await;
        let Ok(OkReply::Message { cid, text, .. }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!((cid, text.as_str()), (Cid(1), "важное"));
        for _ in 0..2 {
            let _ = link
                .execute(Request::UnpinMessage {
                    cid: Cid(1),
                    msg_id: first,
                })
                .await;
        }
        let reply = link.execute(Request::PinnedMessages { cid: Cid(1) }).await;
        assert_eq!(reply, Ok(OkReply::PinnedMessages { ids: vec![second] }));
    }

    /// Визитки и удаление чата (S13): семантика §4.8 — карточка только
    /// по живому соединению, удаление стирает контакт и его закрепы.
    #[tokio::test(start_paused = true)]
    async fn cards_and_delete_match_kernel_semantics() {
        let link = MockLink::new(scene());

        // Отправка: чужая и своя карточки онлайн-контакту — ок.
        let reply = link
            .execute(Request::SendContact {
                to: Cid(1),
                contact: Some(Cid(2)),
            })
            .await;
        assert_eq!(reply, Ok(OkReply::Done));
        let reply = link
            .execute(Request::SendContact {
                to: Cid(1),
                contact: None,
            })
            .await;
        assert_eq!(reply, Ok(OkReply::Done), "своя карточка — contact = None");

        // Ошибки: неизвестные cid, офлайн получателя.
        let reply = link
            .execute(Request::SendContact {
                to: Cid(99),
                contact: None,
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));
        let reply = link
            .execute(Request::SendContact {
                to: Cid(1),
                contact: Some(Cid(99)),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));
        // scene(): контакт 2 офлайн.
        let reply = link
            .execute(Request::SendContact {
                to: Cid(2),
                contact: None,
            })
            .await;
        assert_eq!(
            reply,
            Err(LinkError::Kernel(Status::NoSession)),
            "карточка уходит только по живому соединению"
        );

        // Удаление: контакт исчезает, его закрепы и закреп чата чистятся.
        let _ = link.execute(Request::PinChat { cid: Cid(1) }).await;
        let reply = link.execute(Request::DeleteChat { cid: Cid(1) }).await;
        assert_eq!(reply, Ok(OkReply::Done));
        let reply = link.execute(Request::PinnedChat).await;
        assert_eq!(reply, Ok(OkReply::PinnedChat { cid: None }));
        let reply = link.execute(Request::DeleteChat { cid: Cid(1) }).await;
        assert_eq!(
            reply,
            Err(LinkError::Kernel(Status::NoContact)),
            "повторное удаление — контакта уже нет"
        );
        let Ok(OkReply::Contacts(contacts)) = link.execute(Request::Contacts).await else {
            panic!("contacts")
        };
        assert!(
            contacts.iter().all(|c| c.cid != Cid(1)),
            "удалённый не возвращается в списке"
        );
    }

    /// SET_NAME (имя профиля): 1..=32 символа, иначе BAD_ARGS (§4.4).
    #[tokio::test(start_paused = true)]
    async fn set_name_validates_length() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::SetName {
                name: "Михаил".into(),
            })
            .await;
        assert_eq!(reply, Ok(OkReply::Done));
        let reply = link
            .execute(Request::SetName {
                name: String::new(),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::BadArgs)));
        let reply = link
            .execute(Request::SetName {
                name: "а".repeat(33),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::BadArgs)));
    }

    /// Звонки (S7): одна линия — BUSY, офлайн — NO_SESSION; исходящий
    /// принимается «пиром» через задержку сцены; завершение — end.
    #[tokio::test(start_paused = true)]
    async fn calls_match_kernel_semantics() {
        let link = Arc::new(MockLink::new(scene()));
        let events = link.clone().events();

        // Офлайн-контакт (2) — NO_SESSION, неизвестный — NO_CONTACT.
        let reply = link.execute(Request::StartCall { cid: Cid(2) }).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoSession)));
        let reply = link.execute(Request::StartCall { cid: Cid(99) }).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));

        // Исходящий: ответ с call_id, события offer → accept → end.
        let reply = link.execute(Request::StartCall { cid: Cid(1) }).await;
        let Ok(OkReply::CallStarted { call_id }) = reply else {
            panic!("START_CALL: {reply:?}")
        };
        // Линия занята, пока звонок жив.
        let reply = link.execute(Request::StartCall { cid: Cid(1) }).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::Busy)));

        // Accept по таймеру сцены (50 мс), затем завершение — события
        // копятся в канале, забираем пачкой.
        time::sleep(Duration::from_millis(100)).await;
        let reply = link.execute(Request::EndCall).await;
        assert_eq!(reply, Ok(OkReply::Done));

        let mut got = crate::core_link::take_events(events, 3).await;
        let offer = got.pop_front().expect("offer");
        assert_eq!(
            offer,
            LinkEvent::CallState {
                cid: Cid(1),
                call_id,
                state: CallSignal::Offer,
            }
        );
        let accept = got.pop_front().expect("accept");
        assert_eq!(
            accept,
            LinkEvent::CallState {
                cid: Cid(1),
                call_id,
                state: CallSignal::Accept,
            }
        );
        let end = got.pop_front().expect("end");
        assert_eq!(
            end,
            LinkEvent::CallState {
                cid: Cid(1),
                call_id,
                state: CallSignal::End,
            }
        );
        let reply = link.execute(Request::EndCall).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoCall)));
        let reply = link.execute(Request::AnswerCall).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoCall)));
    }

    /// S14: кривая разметка — BAD_ARGS от заглушки, как у ядра.
    #[tokio::test(start_paused = true)]
    async fn send_with_invalid_entities_is_bad_args() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::SendMsg {
                cid: Cid(1),
                text: "привет".into(),
                entities: vec![Entity::new(EntityKind::Bold, 0, 99)],
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::BadArgs)));
    }

    /// S10: SEND_FILE в заглушке повторяет ядро — путь с диска, честные
    /// FILE_NOT_FOUND/TOO_BIG/BAD_ARGS, FILE_SENT после паузы.
    #[tokio::test(start_paused = true)]
    async fn send_file_mock_semantics() {
        let link = Arc::new(MockLink::new(scene()));
        let events = CoreLink::events(Arc::clone(&link));
        tokio::pin!(events);

        let tmp = std::env::temp_dir().join(format!("shopot-mock-file-{}", std::process::id()));
        std::fs::write(&tmp, b"demo").expect("файл записан");
        let reply = link
            .execute(Request::SendFile {
                cid: Cid(1),
                path: tmp.to_string_lossy().into_owned(),
            })
            .await;
        let Ok(OkReply::FileQueued {
            file_id,
            size,
            chunks,
        }) = reply
        else {
            panic!("ожидали FileQueued: {reply:?}")
        };
        assert_eq!(size, 4);
        assert_eq!(chunks, 1);

        // FILE_SENT приходит после паузы ответа (контакт онлайн).
        use futures_core::Stream;
        let event = tokio::time::timeout(
            Duration::from_secs(5),
            std::future::poll_fn(|cx| events.as_mut().poll_next(cx)),
        )
        .await
        .expect("событие есть")
        .expect("поток жив");
        assert_eq!(
            event,
            LinkEvent::FileSent {
                cid: Cid(1),
                file_id
            }
        );

        // Нет файла — FILE_NOT_FOUND; неизвестный контакт — NO_CONTACT.
        let reply = link
            .execute(Request::SendFile {
                cid: Cid(1),
                path: "/нет/такого.bin".into(),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::FileNotFound)));
        let reply = link
            .execute(Request::SendFile {
                cid: Cid(99),
                path: tmp.to_string_lossy().into_owned(),
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));
        std::fs::remove_file(&tmp).ok();
    }

    /// S8б: SET_AVATAR в заглушке — лимит 16000 Б, пустая — BAD_ARGS;
    /// GET_AVATAR_ID: своя после Set, чужая неизвестная — нули.
    #[tokio::test(start_paused = true)]
    async fn avatar_mock_semantics() {
        let link = MockLink::new(scene());
        let reply = link
            .execute(Request::SetAvatar {
                data: vec![0x89; 100],
            })
            .await;
        let Ok(OkReply::AvatarSet { avatar_id, size }) = reply else {
            panic!("ожидали AvatarSet: {reply:?}")
        };
        assert_eq!(size, 100);
        assert_ne!(avatar_id, [0; 16]);

        let reply = link.execute(Request::GetAvatarId { cid: Cid(0) }).await;
        assert_eq!(reply, Ok(OkReply::AvatarId(avatar_id)));
        let reply = link.execute(Request::GetAvatarId { cid: Cid(1) }).await;
        assert_eq!(reply, Ok(OkReply::AvatarId([0; 16])));

        let reply = link.execute(Request::SetAvatar { data: vec![] }).await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::TooBig)));
        let reply = link
            .execute(Request::SetAvatar {
                data: vec![0x89; 16001],
            })
            .await;
        assert_eq!(reply, Err(LinkError::Kernel(Status::TooBig)));
    }
}
