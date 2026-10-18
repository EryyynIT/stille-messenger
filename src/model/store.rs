//! Store: проекция состояния ядра. cid — единственный ключ.

use std::collections::HashMap;

use crate::core_link::{Entity, HistoryMsg};
use crate::model::cid::Cid;
use crate::model::feed::{self, FeedItem};
use crate::model::msg::{CoreMsgId, Dir, FileRef, Msg, MsgId, Reaction, SendState};
use crate::model::settings::Settings;

/// Контакт.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub cid: Cid,
    pub name: String,
    pub online: bool,
    pub blocked: bool,
    /// avatar_id из профиля (S8б): None — аватарки нет или ещё не
    /// знаем; байты — в avatar_cache стора (ядро дотаскивает само).
    pub avatar_id: Option<[u8; 16]>,
}

/// Совпадение поиска по загруженной ленте (Ctrl+F, S6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub id: MsgId,
    pub ts_ms: i64,
    pub mine: bool,
    pub body: String,
}

/// Закреплённое сообщение (S12): порядок и состав — от ядра
/// (GET_PINNED_MESSAGES), тела дозапрашиваются через GET_MESSAGE.
/// `body = None` — тело в пути; такая запись не рисуется (без заглушек).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinEntry {
    pub msg_id: CoreMsgId,
    pub ts_ms: i64,
    pub mine: bool,
    pub body: Option<String>,
    /// GET_MESSAGE по этому закрепу завершился ошибкой: панель говорит
    /// «не загрузилось», а не висит в вечном «загружается».
    pub failed: bool,
}

/// Состояние защищённой сессии с контактом. Ядро отдельного события сессии
/// не шлёт: сессия = живое соединение, поэтому её определяет PRESENCE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Session {
    #[default]
    Unknown,
    Up,
    Down,
}

/// Лента чата: сообщения, непрочитанные, черновик, сессия
/// (в памяти, не на диске).
#[derive(Clone, Debug, Default)]
pub struct Thread {
    pub msgs: Vec<Msg>,
    pub unread: u32,
    pub draft: String,
    pub session: Session,
    /// GET_HISTORY уже запрашивался: пустой ответ не перезапрашиваем.
    pub history_requested: bool,
    /// Сколько записей ядра загружено в ленту (для окна догрузки).
    pub history_loaded: u32,
    /// Сколько записей у ядра всего (total из ответа GET_HISTORY).
    pub history_total: u32,
    /// Закреплённые сообщения чата в порядке закрепления (S12).
    pub pins: Vec<PinEntry>,
    /// GET_PINNED_MESSAGES уже запрашивался для этого чата.
    pub pins_requested: bool,
}

/// Проекция ядра для UI. Пересобирается из contacts + событий.
#[derive(Debug, Default)]
pub struct Store {
    contacts: HashMap<Cid, Contact>,
    order: Vec<Cid>,
    threads: HashMap<Cid, Thread>,
    selected: Option<Cid>,
    next_msg_id: u64,
    /// Выбор пользователя (масштаб, компакт, приватность уведомлений).
    pub settings: Settings,
    /// Закреплённый чат демона (GET_PINNED_CHAT, API 2.2+): один на демон,
    /// переживает рестарт ядра.
    pinned_chat: Option<Cid>,
    /// Кэш байтов аватарок по avatar_id (S8б, §4.4: клиент кэширует,
    /// ядро доставляет само). В памяти: на диск не пишем (инвариант
    /// §0); после перезапуска ядро пришлёт снова при сессии с пиром.
    avatar_cache: HashMap<[u8; 16], Vec<u8>>,
    /// Своя аватарка (id + байты) — показываем в своей карточке сразу
    /// после SET_AVATAR, не дожидаясь эха.
    own_avatar: Option<([u8; 16], Vec<u8>)>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ресинк контактов из машинного ответа ядра. Ленты не трогает.
    pub fn replace_contacts(&mut self, contacts: Vec<Contact>) {
        self.order = contacts.iter().map(|c| c.cid).collect();
        for contact in contacts {
            self.threads.entry(contact.cid).or_default();
            self.contacts.insert(contact.cid, contact);
        }
        self.order.retain(|cid| self.contacts.contains_key(cid));
    }

    /// Контакты в порядке ядра.
    pub fn contacts(&self) -> impl Iterator<Item = &Contact> + '_ {
        self.order.iter().filter_map(|cid| self.contacts.get(cid))
    }

    pub fn contact(&self, cid: Cid) -> Option<&Contact> {
        self.contacts.get(&cid)
    }

    pub fn online_count(&self) -> usize {
        self.contacts.values().filter(|c| c.online).count()
    }

    pub fn selected(&self) -> Option<Cid> {
        self.selected
    }

    /// Открыть чат: сбрасывает непрочитанные выбранного.
    pub fn select(&mut self, cid: Option<Cid>) {
        self.selected = cid;
        if let Some(cid) = cid {
            if let Some(thread) = self.threads.get_mut(&cid) {
                thread.unread = 0;
            }
        }
    }

    /// Входящее сообщение от известного/нового контакта; имя — из события.
    /// Новые сообщения ядра идут через [`Self::push_incoming_core`].
    pub fn push_incoming(&mut self, cid: Cid, name: &str, msg_id: MsgId, ts_ms: i64, text: String) {
        let msg = Msg::incoming(msg_id, cid, ts_ms, text, None, Vec::new());
        self.push_incoming_impl(cid, name, msg);
    }

    /// Входящее сообщение с настоящим msg_id ядра (событие MESSAGE_RECEIVED).
    pub fn push_incoming_core(
        &mut self,
        cid: Cid,
        name: &str,
        core_id: CoreMsgId,
        ts_ms: i64,
        text: String,
        entities: Vec<Entity>,
    ) {
        let local = self.next_local_id();
        let msg = Msg::incoming(local, cid, ts_ms, text, Some(core_id), entities);
        self.push_incoming_impl(cid, name, msg);
    }

    fn next_local_id(&mut self) -> MsgId {
        self.next_msg_id += 1;
        MsgId(self.next_msg_id)
    }

    fn push_incoming_impl(&mut self, cid: Cid, name: &str, msg: Msg) {
        let fresh = !self.contacts.contains_key(&cid);
        if fresh {
            self.order.push(cid);
            self.threads.entry(cid).or_default();
            self.contacts.insert(
                cid,
                Contact {
                    cid,
                    name: name.to_owned(),
                    online: true,
                    blocked: false,
                    avatar_id: None,
                },
            );
        }
        let unread = if self.selected == Some(cid) { 0 } else { 1 };
        let thread = self.threads.entry(cid).or_default();
        thread.unread += unread;
        thread.msgs.push(msg);
    }

    /// Моё сообщение: сразу `Sending`, идентификатор выдаёт Store.
    pub fn push_outgoing(
        &mut self,
        cid: Cid,
        ts_ms: i64,
        text: String,
        entities: Vec<Entity>,
    ) -> MsgId {
        let id = self.next_local_id();
        self.threads
            .entry(cid)
            .or_default()
            .msgs
            .push(Msg::outgoing(id, ts_ms, text, entities));
        id
    }

    /// Мой файл: Sending до события FILE_SENT (честный статус, S10).
    pub fn push_outgoing_file(&mut self, cid: Cid, ts_ms: i64, file: FileRef) -> MsgId {
        let id = self.next_local_id();
        self.threads
            .entry(cid)
            .or_default()
            .msgs
            .push(Msg::outgoing_file(id, ts_ms, file));
        id
    }

    /// Ответ SEND_FILE: ядро посчитало file_id/size — пузырь получает
    /// настоящие (S10). Локальный стоял с нулевым id.
    pub fn set_file_meta(&mut self, cid: Cid, id: MsgId, file_id: [u8; 16], size: u32) {
        if let Some(msg) = self.msg_mut(cid, id) {
            if let Some(file) = &mut msg.file {
                file.file_id = file_id;
                file.size = size;
            }
        }
    }

    /// FILE_SENT: все чанки подтверждены — пузырь «отправлен».
    pub fn mark_file_sent(&mut self, cid: Cid, file_id: [u8; 16]) {
        let Some(thread) = self.threads.get_mut(&cid) else {
            return;
        };
        for msg in thread.msgs.iter_mut().rev() {
            if msg.file.as_ref().map(|f| f.file_id) == Some(file_id) {
                msg.send = SendState::Sent;
                return;
            }
        }
    }

    /// Входящий файл (FILE_RECEIVED, S10): как входящее сообщение —
    /// непрочитанное, новый контакт создаётся.
    pub fn push_incoming_file(&mut self, cid: Cid, name: &str, ts_ms: i64, file: FileRef) {
        let id = self.next_local_id();
        let msg = Msg::incoming_file(id, cid, ts_ms, file);
        self.push_incoming_impl(cid, name, msg);
    }

    /// AVATAR_RECEIVED (S8б): байты в кэш по id, контакту — avatar_id.
    /// cid 0 — эхо собственной аватарки. Незнакомый cid — кэшируем,
    /// контакт появится по своему событию.
    pub fn apply_avatar(&mut self, cid: Cid, avatar_id: [u8; 16], data: Vec<u8>) {
        // Нулевой id = «аватарки нет»: снимаем с контакта, не кэшируем.
        if avatar_id == [0; 16] {
            if let Some(contact) = self.contacts.get_mut(&cid) {
                contact.avatar_id = None;
            }
            return;
        }
        self.avatar_cache.insert(avatar_id, data.clone());
        if cid.0 == 0 {
            self.own_avatar = Some((avatar_id, data));
        } else if let Some(contact) = self.contacts.get_mut(&cid) {
            contact.avatar_id = Some(avatar_id);
        }
    }

    /// CONTACT_PROFILE: имя и/или avatar_id сменились; байты придут
    /// событием AVATAR_RECEIVED (ядро запрашивает при рассинхроне).
    pub fn apply_profile(&mut self, cid: Cid, name: String, avatar_id: [u8; 16]) {
        if let Some(contact) = self.contacts.get_mut(&cid) {
            if !name.is_empty() {
                contact.name = name;
            }
            contact.avatar_id = (avatar_id != [0; 16]).then_some(avatar_id);
        }
    }

    /// Своя аватарка после SET_AVATAR: показываем сразу.
    pub fn set_own_avatar(&mut self, avatar_id: [u8; 16], data: Vec<u8>) {
        self.avatar_cache.insert(avatar_id, data.clone());
        self.own_avatar = Some((avatar_id, data));
    }

    /// Байты аватарки контакта, если ядро их уже прислало.
    pub fn avatar(&self, cid: Cid) -> Option<&[u8]> {
        let id = self.contacts.get(&cid)?.avatar_id?;
        self.avatar_cache.get(&id).map(Vec::as_slice)
    }

    /// Своя аватарка (для карточки профиля).
    pub fn own_avatar(&self) -> Option<&[u8]> {
        self.own_avatar.as_ref().map(|(_, data)| data.as_slice())
    }

    /// Служебная строка ядра в ленте (контакт подключился, заблокировал…).
    pub fn push_sys(&mut self, cid: Cid, ts_ms: i64, text: String) {
        self.threads
            .entry(cid)
            .or_default()
            .msgs
            .push(Msg::sys(ts_ms, text));
    }

    /// Визитка от собеседника (CONTACT_CARD, S13): событие ядра, записи
    /// в истории нет — переживает только текущий запуск GUI. Считается
    /// непрочитанной, как входящее сообщение.
    pub fn push_card(&mut self, cid: Cid, ts_ms: i64, name: String) {
        if self.selected != Some(cid) {
            self.threads.entry(cid).or_default().unread += 1;
        }
        self.threads
            .entry(cid)
            .or_default()
            .msgs
            .push(Msg::card(ts_ms, name));
    }

    /// DELETE_CHAT отвечен (API 2.2+): убираем контакт и всю его
    /// проекцию — ленту, черновик, закрепы; закреплённый чат ядро
    /// открепило само. Удалённый открытый чат закрывается.
    pub fn delete_chat(&mut self, cid: Cid) {
        self.contacts.remove(&cid);
        self.order.retain(|c| *c != cid);
        self.threads.remove(&cid);
        if self.pinned_chat == Some(cid) {
            self.pinned_chat = None;
        }
        if self.selected == Some(cid) {
            self.selected = None;
        }
    }

    /// Состояние сессии с контактом — для подзаголовка шапки чата.
    pub fn session(&self, cid: Cid) -> Session {
        self.threads
            .get(&cid)
            .map(|t| t.session)
            .unwrap_or(Session::Unknown)
    }

    pub fn mark_sent(&mut self, cid: Cid, msg_id: MsgId) {
        self.set_send_state(cid, msg_id, SendState::Sent);
    }

    /// Ответ SEND_MESSAGE: сообщение ушло в сессию, ядровый msg_id привязан.
    pub fn mark_sent_core(&mut self, cid: Cid, msg_id: MsgId, core_id: CoreMsgId) {
        if let Some(msg) = self.msg_mut(cid, msg_id) {
            msg.core_id = Some(core_id);
            msg.send = SendState::Sent;
        }
    }

    /// Ответ SEND_MESSAGE с queued=1: сообщение в outbox, уедет при онлайне.
    pub fn mark_queued(&mut self, cid: Cid, msg_id: MsgId) {
        self.set_send_state(cid, msg_id, SendState::Queued);
    }

    pub fn mark_failed(&mut self, cid: Cid, msg_id: MsgId, reason: String) {
        self.set_send_state(cid, msg_id, SendState::Failed(reason));
    }

    /// Событие MESSAGE_DELIVERED: пир подтвердил получение по msg_id ядра.
    /// Применить реакцию к сообщению (S15): mine=false — событие
    /// REACTION от собеседника, mine=true — свой SET/REMOVE после OK.
    /// Незнакомый msg_id (сообщение не загружено) — игнор: истории
    /// реакций у ядра нет, восстанавливать нечего. Возвращает, нашлось
    /// ли сообщение.
    pub fn apply_reaction(
        &mut self,
        cid: Cid,
        core_id: CoreMsgId,
        add: bool,
        mine: bool,
        emoji: char,
    ) -> bool {
        let Some(thread) = self.threads.get_mut(&cid) else {
            return false;
        };
        let Some(msg) = thread
            .msgs
            .iter_mut()
            .rev()
            .find(|m| m.core_id == Some(core_id))
        else {
            return false;
        };
        if add {
            match msg.reactions.iter_mut().find(|r| r.emoji == emoji) {
                Some(r) => {
                    r.count += 1;
                    r.mine |= mine;
                }
                None => msg.reactions.push(Reaction {
                    emoji,
                    count: 1,
                    mine,
                }),
            }
        } else {
            if let Some(r) = msg.reactions.iter_mut().find(|r| r.emoji == emoji) {
                r.count = r.count.saturating_sub(1);
                if mine {
                    r.mine = false;
                }
            }
            msg.reactions.retain(|r| r.count > 0);
        }
        true
    }

    pub fn mark_delivered(&mut self, cid: Cid, core_id: CoreMsgId) {
        let Some(thread) = self.threads.get_mut(&cid) else {
            return;
        };
        for msg in thread.msgs.iter_mut().rev() {
            if msg.core_id == Some(core_id) {
                msg.send = SendState::Delivered;
                return;
            }
        }
    }

    fn msg_mut(&mut self, cid: Cid, msg_id: MsgId) -> Option<&mut Msg> {
        self.threads
            .get_mut(&cid)?
            .msgs
            .iter_mut()
            .rev()
            .find(|m| m.id == msg_id)
    }

    fn set_send_state(&mut self, cid: Cid, msg_id: MsgId, send: SendState) {
        if let Some(msg) = self.msg_mut(cid, msg_id) {
            msg.send = send;
        }
    }

    /// PRESENCE ядра: онлайн-флаг и производная от него сессия.
    pub fn set_presence(&mut self, cid: Cid, online: bool) {
        if let Some(contact) = self.contacts.get_mut(&cid) {
            contact.online = online;
        }
        if let Some(thread) = self.threads.get_mut(&cid) {
            thread.session = if online { Session::Up } else { Session::Down };
        }
    }

    /// Контакт появился (CONTACT_JOINED): создаём, не трогая существующего.
    pub fn ensure_contact(&mut self, cid: Cid, name: &str) -> bool {
        if self.contacts.contains_key(&cid) {
            return false;
        }
        self.order.push(cid);
        self.threads.entry(cid).or_default();
        self.contacts.insert(
            cid,
            Contact {
                cid,
                name: name.to_owned(),
                online: false,
                blocked: false,
                avatar_id: None,
            },
        );
        true
    }

    /// CONTACT_PROFILE: имя контакта обновилось.
    pub fn rename_contact(&mut self, cid: Cid, name: &str) {
        if let Some(contact) = self.contacts.get_mut(&cid) {
            contact.name = name.to_owned();
        }
    }

    /// Ответ BLOCK_CONTACT/UNBLOCK_CONTACT: флаг блокировки контакта.
    pub fn set_blocked(&mut self, cid: Cid, blocked: bool) {
        if let Some(contact) = self.contacts.get_mut(&cid) {
            contact.blocked = blocked;
        }
    }

    pub fn thread(&self, cid: Cid) -> Option<&Thread> {
        self.threads.get(&cid)
    }

    pub fn unread_total(&self) -> u32 {
        self.threads.values().map(|t| t.unread).sum()
    }

    pub fn draft(&self, cid: Cid) -> &str {
        self.threads
            .get(&cid)
            .map(|t| t.draft.as_str())
            .unwrap_or("")
    }

    pub fn set_draft(&mut self, cid: Cid, draft: String) {
        self.threads.entry(cid).or_default().draft = draft;
    }

    /// Лента чата, сгруппированная для отображения; имена авторов
    /// подставлены из контактов.
    pub fn feed(&self, cid: Cid, day_of: impl Fn(i64) -> i64) -> Vec<FeedItem> {
        let Some(thread) = self.threads.get(&cid) else {
            return Vec::new();
        };
        let mut items = feed::group(&thread.msgs, day_of);
        for item in items.iter_mut() {
            let FeedItem::Group { author, name, .. } = item else {
                continue;
            };
            let Some(author) = *author else { continue };
            *name = self
                .contacts
                .get(&author)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| author.to_string());
        }
        items
    }

    /// GET_HISTORY запрошен: пустой ответ не перезапрашиваем при
    /// повторном открытии чата.
    pub fn mark_history_requested(&mut self, cid: Cid) {
        self.threads.entry(cid).or_default().history_requested = true;
    }

    pub fn history_requested(&self, cid: Cid) -> bool {
        self.threads.get(&cid).is_some_and(|t| t.history_requested)
    }

    /// Сколько записей ядра уже загружено в ленту чата.
    pub fn history_loaded(&self, cid: Cid) -> u32 {
        self.threads
            .get(&cid)
            .map(|t| t.history_loaded)
            .unwrap_or(0)
    }

    /// Есть ли в ядре ещё незагруженные сообщения. Потолок окна ядра
    /// (HISTORY_MAX_LIMIT) останавливает догрузку: глубже окно не растёт.
    pub fn has_more_history(&self, cid: Cid) -> bool {
        let Some(thread) = self.threads.get(&cid) else {
            return false;
        };
        thread.history_loaded < thread.history_total
            && thread.history_loaded < crate::core_link::HISTORY_MAX_LIMIT
    }

    /// Ответ GET_HISTORY: записи дописываются В НАЧАЛО ленты (окно ядра —
    /// новейшие, порядок старые → новые). Пересечения с живой лентой и
    /// внутри партии дедуплицируются по core_id. Свои записи с ACK пира
    /// восстанавливаются как «доставлено».
    pub fn load_history(&mut self, cid: Cid, total: u32, records: &[HistoryMsg]) {
        let known: std::collections::HashSet<CoreMsgId> = self
            .threads
            .get(&cid)
            .map(|t| t.msgs.iter().filter_map(|m| m.core_id).collect())
            .unwrap_or_default();
        let mut batch: std::collections::HashSet<CoreMsgId> = std::collections::HashSet::new();
        let mut fresh: Vec<Msg> = Vec::with_capacity(records.len());
        for record in records {
            let core_id = record.msg_id;
            if known.contains(&core_id) || !batch.insert(core_id) {
                continue;
            }
            let msg = if record.mine {
                let mut msg = Msg::outgoing(
                    self.next_local_id(),
                    record.ts_ms,
                    record.text.clone(),
                    record.entities.clone(),
                );
                msg.core_id = Some(core_id);
                msg.send = if record.delivered {
                    SendState::Delivered
                } else {
                    SendState::Sent
                };
                msg
            } else {
                Msg::incoming(
                    self.next_local_id(),
                    cid,
                    record.ts_ms,
                    record.text.clone(),
                    Some(core_id),
                    record.entities.clone(),
                )
            };
            fresh.push(msg);
        }
        let thread = self.threads.entry(cid).or_default();
        thread.history_loaded += fresh.len() as u32;
        thread.history_total = thread.history_total.max(total);
        let old = std::mem::take(&mut thread.msgs);
        thread.msgs = fresh;
        thread.msgs.extend(old);
    }

    /// Поиск по загруженной ленте чата (Ctrl+F, S6): регистр не важен,
    /// служебные строки и визитки не участвуют. Ядро не спрашивается —
    /// серверный поиск появится со своим спринтом.
    pub fn search_loaded(&self, cid: Cid, query: &str) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let Some(thread) = self.threads.get(&cid) else {
            return Vec::new();
        };
        thread
            .msgs
            .iter()
            .filter(|m| !matches!(m.dir, Dir::Sys | Dir::Card))
            .filter(|m| m.body.to_lowercase().contains(&needle))
            .map(|m| SearchHit {
                id: m.id,
                ts_ms: m.ts_ms,
                mine: matches!(m.dir, Dir::Out),
                body: m.body.clone(),
            })
            .collect()
    }

    /// Закреплённый чат (GET_PINNED_CHAT, S12).
    pub fn pinned_chat(&self) -> Option<Cid> {
        self.pinned_chat
    }

    pub fn set_pinned_chat(&mut self, cid: Option<Cid>) {
        self.pinned_chat = cid;
    }

    /// Закрепы чата от ядра (GET_PINNED_MESSAGES): порядок закрепления.
    /// Уже разрешённые тела сохраняются — повторный ответ не сбрасывает
    /// их в «незагружено»; тело нового закрепа подхватывается из
    /// загруженной ленты, иначе дозапрашивается (GET_MESSAGE).
    pub fn set_pin_ids(&mut self, cid: Cid, ids: &[CoreMsgId]) {
        let thread = self.threads.entry(cid).or_default();
        let old = std::mem::take(&mut thread.pins);
        thread.pins = ids
            .iter()
            .map(|id| {
                let resolved = old
                    .iter()
                    .find(|p| p.msg_id == *id)
                    .and_then(|p| p.body.clone().map(|body| (p.ts_ms, p.mine, body)))
                    .or_else(|| {
                        let msg = thread.msgs.iter().find(|m| m.core_id == Some(*id))?;
                        Some((msg.ts_ms, matches!(msg.dir, Dir::Out), msg.body.clone()))
                    });
                match resolved {
                    Some((ts_ms, mine, body)) => PinEntry {
                        msg_id: *id,
                        ts_ms,
                        mine,
                        body: Some(body),
                        failed: false,
                    },
                    None => PinEntry {
                        msg_id: *id,
                        ts_ms: 0,
                        mine: false,
                        body: None,
                        failed: false,
                    },
                }
            })
            .collect();
    }

    /// Тело закрепа от GET_MESSAGE; неизвестный msg_id — не наш ответ.
    pub fn fill_pin(&mut self, cid: Cid, msg_id: CoreMsgId, ts_ms: i64, mine: bool, body: String) {
        let Some(thread) = self.threads.get_mut(&cid) else {
            return;
        };
        if let Some(entry) = thread.pins.iter_mut().find(|p| p.msg_id == msg_id) {
            *entry = PinEntry {
                msg_id,
                ts_ms,
                mine,
                body: Some(body),
                failed: false,
            };
        }
    }

    /// GET_MESSAGE по закрепу упал: запись помечается, панель перестаёт
    /// показывать «загружается». Возвращает чат-владельца закрепа.
    pub fn fail_pin(&mut self, msg_id: CoreMsgId) -> Option<Cid> {
        for (cid, thread) in &mut self.threads {
            if let Some(entry) = thread.pins.iter_mut().find(|p| p.msg_id == msg_id) {
                entry.failed = true;
                return Some(*cid);
            }
        }
        None
    }

    /// Обратный поиск: msg_id ядра → локальный id в загруженной ленте
    /// (переход из поиска/закрепов к сообщению, S16).
    pub fn local_id_of_core(&self, cid: Cid, core_id: CoreMsgId) -> Option<MsgId> {
        self.threads
            .get(&cid)?
            .msgs
            .iter()
            .find(|m| m.core_id == Some(core_id))
            .map(|m| m.id)
    }

    /// Закрепы чата для UI (порядок закрепления).
    pub fn pins(&self, cid: Cid) -> &[PinEntry] {
        self.threads
            .get(&cid)
            .map_or(&[], |thread| thread.pins.as_slice())
    }

    /// Закреплено ли сообщение (для пункта меню пузыря).
    pub fn is_pinned(&self, cid: Cid, msg_id: CoreMsgId) -> bool {
        self.pins(cid).iter().any(|p| p.msg_id == msg_id)
    }

    /// msg_id ядра для строки ленты (ПКМ по пузырю): известен не всем —
    /// своим до ответа SEND_MESSAGE его ещё нет.
    pub fn core_id_of(&self, cid: Cid, id: MsgId) -> Option<CoreMsgId> {
        self.msg(cid, id).and_then(|m| m.core_id)
    }

    /// Строка ленты по локальному id (меню пузыря, S12).
    pub fn msg(&self, cid: Cid, id: MsgId) -> Option<&Msg> {
        self.threads.get(&cid)?.msgs.iter().find(|m| m.id == id)
    }

    /// GET_PINNED_MESSAGES уже запрашивался для чата.
    pub fn pins_requested(&self, cid: Cid) -> bool {
        self.threads
            .get(&cid)
            .map(|t| t.pins_requested)
            .unwrap_or(false)
    }

    pub fn mark_pins_requested(&mut self, cid: Cid) {
        self.threads.entry(cid).or_default().pins_requested = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_drives_session_state() {
        let mut store = Store::default();
        store.replace_contacts(vec![contact(1, "Аня", false)]);
        assert_eq!(store.session(Cid(1)), Session::Unknown);
        store.set_presence(Cid(1), true);
        assert_eq!(store.session(Cid(1)), Session::Up);
        store.set_presence(Cid(1), false);
        assert_eq!(store.session(Cid(1)), Session::Down);
    }

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn contact(cid: i64, name: &str, online: bool) -> Contact {
        Contact {
            cid: Cid(cid),
            name: name.to_owned(),
            online,
            blocked: false,
            avatar_id: None,
        }
    }

    #[test]
    fn replace_contacts_keeps_kernel_order_and_threads() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true), contact(2, "Борис", false)]);
        let names: Vec<&str> = store.contacts().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Аня", "Борис"]);
        assert_eq!(store.online_count(), 1);

        store.push_incoming(Cid(2), "Борис", MsgId(1), 1000, "привет".into());
        store.replace_contacts(vec![contact(1, "Аня", false), contact(2, "Борис", true)]);
        assert_eq!(
            store.thread(Cid(2)).unwrap().unread,
            1,
            "ресинк не трогает ленту"
        );
    }

    #[test]
    fn incoming_to_selected_chat_is_read() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        store.select(Some(Cid(1)));
        store.push_incoming(Cid(1), "Аня", MsgId(1), 1000, "привет".into());
        assert_eq!(store.unread_total(), 0);

        store.select(None);
        store.push_incoming(Cid(1), "Аня", MsgId(2), 2000, "как дела".into());
        assert_eq!(store.unread_total(), 1);
        store.select(Some(Cid(1)));
        assert_eq!(
            store.unread_total(),
            0,
            "открытие чата читает непрочитанные"
        );
    }

    #[test]
    fn outgoing_send_states() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        let id = store.push_outgoing(Cid(1), 1000, "привет".into(), Vec::new());
        let msgs = &store.thread(Cid(1)).unwrap().msgs;
        assert_eq!(msgs[0].send, SendState::Sending);

        store.mark_failed(Cid(1), id, "нет связи".into());
        assert_eq!(
            store.thread(Cid(1)).unwrap().msgs[0].send,
            SendState::Failed("нет связи".into())
        );
        store.mark_sent(Cid(1), id);
        assert_eq!(store.thread(Cid(1)).unwrap().msgs[0].send, SendState::Sent);
    }

    /// Жизненный цикл по модели доставки ядра (CORE-API.md §6):
    /// отправка → queued (контакт офлайн) → sent (ушло) → delivered (ACK).
    #[test]
    fn delivery_lifecycle_queued_sent_delivered() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", false)]);

        let id = store.push_outgoing(Cid(1), 1000, "привет".into(), Vec::new());
        store.mark_queued(Cid(1), id);
        assert_eq!(
            store.thread(Cid(1)).unwrap().msgs[0].send,
            SendState::Queued,
            "офлайн-отправка — это outbox, не ошибка"
        );

        let core = CoreMsgId([0xAA; 16]);
        store.mark_sent_core(Cid(1), id, core);
        assert_eq!(store.thread(Cid(1)).unwrap().msgs[0].send, SendState::Sent);
        assert_eq!(store.thread(Cid(1)).unwrap().msgs[0].core_id, Some(core));

        store.mark_delivered(Cid(1), core);
        assert_eq!(
            store.thread(Cid(1)).unwrap().msgs[0].send,
            SendState::Delivered
        );
    }

    #[test]
    fn delivered_matches_by_core_id_of_mine_and_theirs() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        let mine = CoreMsgId([1; 16]);
        let theirs = CoreMsgId([2; 16]);

        let id = store.push_outgoing(Cid(1), 1000, "моё".into(), Vec::new());
        store.mark_sent_core(Cid(1), id, mine);
        store.push_incoming_core(Cid(1), "Аня", theirs, 1100, "чужое".into(), Vec::new());

        store.mark_delivered(Cid(1), theirs);
        assert_eq!(
            store.thread(Cid(1)).unwrap().msgs[0].send,
            SendState::Sent,
            "чужой msg_id не трогает моё"
        );
        store.mark_delivered(Cid(1), mine);
        assert_eq!(
            store.thread(Cid(1)).unwrap().msgs[0].send,
            SendState::Delivered
        );
    }

    #[test]
    fn contact_joined_creates_and_profile_renames() {
        let mut store = Store::new();
        assert!(store.ensure_contact(Cid(7), "Гость"));
        assert!(store.contact(Cid(7)).is_some());
        assert!(
            !store.ensure_contact(Cid(7), "Дубль"),
            "существующий не трогаем"
        );

        store.rename_contact(Cid(7), "Настоящее имя");
        assert_eq!(store.contact(Cid(7)).unwrap().name, "Настоящее имя");
    }

    #[test]
    fn block_unblock_sets_flag() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        assert!(!store.contact(Cid(1)).unwrap().blocked);

        store.set_blocked(Cid(1), true);
        assert!(store.contact(Cid(1)).unwrap().blocked);
        store.set_blocked(Cid(1), false);
        assert!(!store.contact(Cid(1)).unwrap().blocked);
        // Неизвестный cid молча игнорируется — это данные, не баг.
        store.set_blocked(Cid(99), true);
    }

    #[test]
    fn drafts_survive_chat_switch() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true), contact(2, "Борис", true)]);
        store.set_draft(Cid(1), "черновик Ане".into());
        assert_eq!(store.draft(Cid(1)), "черновик Ане");
        assert_eq!(store.draft(Cid(2)), "");
        store.set_draft(Cid(1), "".into());
        assert_eq!(store.draft(Cid(1)), "");
    }

    fn history(id: u8, mine: bool, delivered: bool, ts: i64, text: &str) -> HistoryMsg {
        HistoryMsg {
            mine,
            delivered,
            msg_id: CoreMsgId([id; 16]),
            ts_ms: ts,
            text: text.to_owned(),
            entities: Vec::new(),
        }
    }

    /// Перезапуск GUI: лента пуста, GET_HISTORY восстанавливает переписку.
    /// Свои доставленные — со статусом «доставлено».
    #[test]
    fn history_restores_feed_after_restart() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        store.load_history(
            Cid(1),
            3,
            &[
                history(1, false, false, 1000, "старое чужое"),
                history(2, true, true, 2000, "старое моё"),
            ],
        );
        let thread = store.thread(Cid(1)).unwrap();
        assert_eq!(thread.msgs.len(), 2);
        assert_eq!(
            thread.msgs[0].body, "старое чужое",
            "порядок старые → новые"
        );
        assert_eq!(thread.msgs[1].send, SendState::Delivered);
        assert_eq!(thread.history_loaded, 2);
        assert_eq!(thread.history_total, 3);
        assert!(store.has_more_history(Cid(1)), "total 3 > loaded 2");

        // Догрузка: дубликат уже загруженного отбрасывается, свежее
        // встаёт перед живой лентой.
        store.load_history(
            Cid(1),
            3,
            &[
                history(0, true, false, 500, "самое старое"),
                history(1, false, false, 1000, "старое чужое"),
            ],
        );
        let msgs = &store.thread(Cid(1)).unwrap().msgs;
        assert_eq!(
            msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            ["самое старое", "старое чужое", "старое моё"]
        );
        assert_eq!(store.thread(Cid(1)).unwrap().history_loaded, 3);
        assert!(!store.has_more_history(Cid(1)), "всё загружено");
    }

    /// Пересечение окна догрузки с сообщениями, пришедшими живьём:
    /// дедупликация по core_id, статусы живых не трогаются.
    #[test]
    fn history_dedupes_against_live_messages() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        let core = CoreMsgId([7; 16]);
        let live = store.push_outgoing(Cid(1), 3000, "живое моё".into(), Vec::new());
        store.mark_sent_core(Cid(1), live, core);

        store.load_history(Cid(1), 1, &[history(7, true, false, 3000, "живое моё")]);
        let msgs = &store.thread(Cid(1)).unwrap().msgs;
        assert_eq!(msgs.len(), 1, "дубликат ядра не добавлен");
        assert_eq!(msgs[0].id, live, "живая строка сохранена");
        assert_eq!(msgs[0].send, SendState::Sent, "статус не перезаписан");
        assert_eq!(store.thread(Cid(1)).unwrap().history_loaded, 0);
        assert_eq!(store.history_loaded(Cid(1)), 0);
    }

    /// S16: msg_id ядра → локальный id загруженного сообщения (переход
    /// из поиска/закрепов). Незагруженное сообщение — None.
    #[test]
    fn core_id_maps_back_to_loaded_message() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        store.load_history(Cid(1), 2, &[history(7, false, false, 1000, "раз")]);

        let local = store.thread(Cid(1)).unwrap().msgs[0].id;
        assert_eq!(
            store.local_id_of_core(Cid(1), CoreMsgId([7; 16])),
            Some(local)
        );
        assert_eq!(store.local_id_of_core(Cid(1), CoreMsgId([9; 16])), None);
        assert_eq!(store.local_id_of_core(Cid(2), CoreMsgId([7; 16])), None);
    }

    /// S16: ошибка GET_MESSAGE помечает закреп — панель перестаёт
    /// висеть в «загружается»; чужой msg_id молча игнорируется.
    #[test]
    fn failed_pin_is_marked_and_owned() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        store.set_pin_ids(Cid(1), &[CoreMsgId([3; 16])]);

        assert_eq!(store.fail_pin(CoreMsgId([3; 16])), Some(Cid(1)));
        assert!(store.pins(Cid(1))[0].failed);
        assert_eq!(store.fail_pin(CoreMsgId([9; 16])), None);

        // Удачный GET_MESSAGE снимает пометку.
        store.fill_pin(Cid(1), CoreMsgId([3; 16]), 1000, false, "тело".into());
        assert!(!store.pins(Cid(1))[0].failed);
        assert_eq!(store.pins(Cid(1))[0].body.as_deref(), Some("тело"));
    }

    /// Потолок окна ядра: loaded == 200 при большем total — догрузки нет.
    #[test]
    fn history_pagination_stops_at_kernel_window_cap() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        let records: Vec<HistoryMsg> = (0..200u8)
            .map(|n| history(n, false, false, i64::from(n), "текст"))
            .collect();
        store.load_history(Cid(1), 500, &records);
        assert_eq!(store.history_loaded(Cid(1)), 200);
        assert!(
            !store.has_more_history(Cid(1)),
            "глубже окна ядра GUI не грузит: total 500, loaded 200"
        );
    }

    /// Пустой ответ не перезапрашивается при каждом открытии чата.
    #[test]
    fn empty_history_is_not_rerequested() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        assert!(!store.history_requested(Cid(1)));
        store.mark_history_requested(Cid(1));
        assert!(store.history_requested(Cid(1)));
        store.load_history(Cid(1), 0, &[]);
        assert!(store.history_requested(Cid(1)));
        assert!(!store.has_more_history(Cid(1)));
    }

    /// Ctrl+F по загруженному: регистр не важен, sys-строки не ищутся.
    #[test]
    fn search_loaded_matches_substring_case_insensitive() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        store.push_incoming(Cid(1), "Аня", MsgId(1), 1000, "Привет, мир".into());
        store.push_outgoing(Cid(1), 2000, "привет из дома".into(), Vec::new());
        store.push_sys(Cid(1), 2500, "Аня подключилась".into());
        store.push_incoming(Cid(1), "Аня", MsgId(2), 3000, "как дела?".into());

        let hits = store.search_loaded(Cid(1), "ПРИВЕТ");
        assert_eq!(hits.len(), 2, "регистр не важен, sys не ищется");
        assert_eq!(hits[0].ts_ms, 1000, "порядок ленты");
        assert!(!hits[0].mine);
        assert!(hits[1].mine);

        // Часть слова тоже находится.
        assert_eq!(store.search_loaded(Cid(1), "дел").len(), 1);
        // Пустой запрос и неизвестный чат — пустой результат.
        assert!(store.search_loaded(Cid(1), "  ").is_empty());
        assert!(store.search_loaded(Cid(9), "привет").is_empty());
    }

    #[test]
    fn feed_groups_by_author_gap_and_day() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        // Вчерашний день, Аня, два подряд; потом мой ответ через 6 минут
        // (новая группа), потом новое сутки — новый разделитель.
        let yesterday = DAY;
        store.push_incoming(Cid(1), "Аня", MsgId(1), yesterday, "привет".into());
        store.push_incoming(Cid(1), "Аня", MsgId(2), yesterday + 1000, "как дела".into());
        store.push_outgoing(Cid(1), yesterday + 6 * 60 * 1000, "норм".into(), Vec::new());
        store.push_incoming(Cid(1), "Аня", MsgId(3), yesterday + DAY, "ок".into());

        let items = store.feed(Cid(1), |ts| ts / DAY);
        assert!(matches!(items[0], FeedItem::DateSep { .. }));
        let FeedItem::Group {
            mine, bodies, name, ..
        } = &items[1]
        else {
            panic!("группа")
        };
        assert!(!*mine);
        assert_eq!(bodies.len(), 2);
        assert_eq!(name, "Аня", "имя автора подставлено из контакта");
        let FeedItem::Group { mine, bodies, .. } = &items[2] else {
            panic!("группа")
        };
        assert!(mine, "смена автора рвёт группу");
        assert_eq!(bodies.len(), 1);
        assert!(matches!(items[3], FeedItem::DateSep { .. }));
        assert_eq!(items.len(), 5);
    }

    /// Закрепы (S12): повторный состав сохраняет разрешённые тела,
    /// новый закреп подхватывает тело из загруженной ленты, неизвестный
    /// ждёт GET_MESSAGE.
    #[test]
    fn pins_resolve_and_survive_refresh() {
        let mut store = Store::default();
        store.replace_contacts(vec![contact(1, "Аня", true)]);
        let in_feed = CoreMsgId([1; 16]);
        let known = CoreMsgId([2; 16]);
        let fresh = CoreMsgId([3; 16]);

        store.load_history(
            Cid(1),
            1,
            &[HistoryMsg {
                mine: false,
                delivered: false,
                msg_id: in_feed,
                ts_ms: 1_000,
                text: "в ленте".into(),
                entities: Vec::new(),
            }],
        );
        store.set_pin_ids(Cid(1), &[known]);
        assert_eq!(store.pins(Cid(1))[0].body, None, "тела пока нет");
        store.fill_pin(Cid(1), known, 500, true, "старое".into());

        // Новый состав: known сохраняет тело, in_feed берёт из ленты,
        // fresh ждёт GET_MESSAGE.
        store.set_pin_ids(Cid(1), &[known, in_feed, fresh]);
        let pins = store.pins(Cid(1));
        assert_eq!(pins.len(), 3, "порядок закрепления сохранён");
        assert_eq!(pins[0].body.as_deref(), Some("старое"));
        assert_eq!(pins[1].body.as_deref(), Some("в ленте"));
        assert_eq!(pins[2].body, None);
        assert!(store.is_pinned(Cid(1), fresh));
        assert!(!store.is_pinned(Cid(1), CoreMsgId([9; 16])));

        // Откреп: состав без known.
        store.set_pin_ids(Cid(1), &[in_feed, fresh]);
        assert!(!store.is_pinned(Cid(1), known));
    }

    /// Визитка (S13): отдельный элемент ленты, непрочитанная в фоновом
    /// чате, в поиске по загруженному не участвует.
    #[test]
    fn card_lands_in_feed_and_counts_unread() {
        let mut store = Store::default();
        store.replace_contacts(vec![contact(1, "Аня", true), contact(2, "Борис", true)]);
        store.select(Some(Cid(1)));

        store.push_card(Cid(2), 1_000, "Вера".into());
        assert_eq!(
            store.thread(Cid(2)).unwrap().unread,
            1,
            "фоновая — непрочитанная"
        );
        let items = store.feed(Cid(2), |ts| ts / DAY);
        assert!(
            matches!(&items[1], FeedItem::Card { name, .. } if name == "Вера"),
            "визитка в ленте после разделителя дня"
        );
        assert!(
            store.search_loaded(Cid(2), "вера").is_empty(),
            "визитка не ищется, как у ядра"
        );

        // Открытая — сразу прочитанная.
        store.push_card(Cid(1), 2_000, "Вера".into());
        assert_eq!(store.thread(Cid(1)).unwrap().unread, 0);
    }

    /// Удаление чата (S13): контакт, лента и черновик исчезают, закреп
    /// чата сброшен, открытый удалённый чат закрывается.
    #[test]
    fn delete_chat_drops_projection() {
        let mut store = Store::default();
        store.replace_contacts(vec![contact(1, "Аня", true), contact(2, "Борис", true)]);
        store.push_incoming(Cid(1), "Аня", MsgId(1), 1_000, "привет".into());
        store.set_draft(Cid(1), "черновик".into());
        store.set_pinned_chat(Some(Cid(1)));
        store.select(Some(Cid(1)));

        store.delete_chat(Cid(1));
        assert!(store.contact(Cid(1)).is_none());
        assert_eq!(
            store.contacts().map(|c| c.cid).collect::<Vec<_>>(),
            vec![Cid(2)],
            "порядок сохранён без удалённого"
        );
        assert!(store.thread(Cid(1)).is_none(), "лента и черновик стёрты");
        assert_eq!(store.pinned_chat(), None, "закреп сброшен, как у ядра");
        assert_eq!(store.selected(), None, "открытый чат закрыт");

        // Повторное удаление и удаление чужого — тихо.
        store.delete_chat(Cid(1));
        store.delete_chat(Cid(99));
        assert_eq!(store.selected(), None);
    }

    /// Реакции (S15): peer add → счётчик; своя — mine; снятие убывает,
    /// ноль — строка исчезает; незнакомый msg_id — игнор.
    #[test]
    fn reactions_apply_and_fade() {
        let mut store = Store::new();
        let cid = Cid(1);
        store.replace_contacts(vec![contact(1, "Аня", false)]);
        let core = CoreMsgId([0x77; 16]);
        store.push_incoming_core(cid, "Аня", core, 1000, "привет".into(), Vec::new());

        // Два события от пира.
        assert!(store.apply_reaction(cid, core, true, false, '👍'));
        assert!(store.apply_reaction(cid, core, true, false, '👍'));
        // Своя поверх.
        assert!(store.apply_reaction(cid, core, true, true, '👍'));
        let reactions = &store.thread(cid).expect("чат есть").msgs[0].reactions;
        assert_eq!(
            reactions,
            &[Reaction {
                emoji: '👍',
                count: 3,
                mine: true
            }]
        );

        // Снял свою: mine=false, счётчик 2, строка жива.
        assert!(store.apply_reaction(cid, core, false, true, '👍'));
        let reactions = &store.thread(cid).expect("чат есть").msgs[0].reactions;
        assert_eq!(
            reactions,
            &[Reaction {
                emoji: '👍',
                count: 2,
                mine: false
            }]
        );

        // Пир снял обе: строки нет.
        assert!(store.apply_reaction(cid, core, false, false, '👍'));
        assert!(store.apply_reaction(cid, core, false, false, '👍'));
        assert!(store.thread(cid).expect("чат есть").msgs[0]
            .reactions
            .is_empty());

        // Снятие без постановки — не уходит в минус; чужой msg_id — игнор.
        assert!(store.apply_reaction(cid, core, false, false, '🔥'));
        assert!(!store.apply_reaction(cid, CoreMsgId([0xEE; 16]), true, false, '🔥'));
    }

    /// Файлы (S10): исходящий Sending → FILE_SENT делает Sent; входящий
    /// создаёт контакт и непрочитанное.
    #[test]
    fn file_lifecycle_honest_status() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", false)]);
        let file = FileRef {
            name: "макет.pdf".into(),
            size: 254_000,
            file_id: [0xF1; 16],
            path: Some("/tmp/макет.pdf".into()),
        };
        let id = store.push_outgoing_file(Cid(1), 1000, file.clone());
        let msgs = &store.thread(Cid(1)).expect("чат есть").msgs;
        assert_eq!(msgs[0].id, id);
        assert_eq!(msgs[0].send, SendState::Sending);
        assert_eq!(msgs[0].file.as_ref().map(|f| f.size), Some(254_000));

        store.mark_file_sent(Cid(1), [0xF1; 16]);
        assert_eq!(store.thread(Cid(1)).unwrap().msgs[0].send, SendState::Sent);
        // Чужой file_id — ничего не трогает.
        store.mark_file_sent(Cid(1), [0xEE; 16]);
        assert_eq!(store.thread(Cid(1)).unwrap().msgs[0].send, SendState::Sent);

        // Входящий от незнакомого cid — контакт появляется, непрочитанное.
        store.push_incoming_file(
            Cid(9),
            "Гриша",
            2000,
            FileRef {
                name: "смета.xlsx".into(),
                size: 12_000,
                file_id: [0xA9; 16],
                path: Some("/home/u/.shopot/files/a9a9".into()),
            },
        );
        assert_eq!(
            store.contact(Cid(9)).map(|c| c.name.as_str()),
            Some("Гриша")
        );
        assert_eq!(store.thread(Cid(9)).unwrap().unread, 1);
    }

    /// Аватарки (S8б): байты в кэш по id, контакту — avatar_id; cid 0 —
    /// своя; профиль без байтов только помечает id; чужой cid без
    /// контакта кэширует байты на будущее.
    #[test]
    fn avatar_cache_by_id() {
        let mut store = Store::new();
        store.replace_contacts(vec![contact(1, "Аня", false)]);
        assert!(store.avatar(Cid(1)).is_none());
        assert!(store.own_avatar().is_none());

        store.apply_avatar(Cid(1), [0xA1; 16], vec![1, 2, 3]);
        assert_eq!(store.avatar(Cid(1)), Some(&[1, 2, 3][..]));

        // Профиль со сменой id: байты ещё не пришли — аватарки нет, но
        // id обновлён (кэш по новому id пуст до события).
        store.apply_profile(Cid(1), "Анна".into(), [0xB2; 16]);
        assert_eq!(store.contact(Cid(1)).unwrap().name, "Анна");
        assert!(store.avatar(Cid(1)).is_none());
        store.apply_avatar(Cid(1), [0xB2; 16], vec![9]);
        assert_eq!(store.avatar(Cid(1)), Some(&[9][..]));

        // Своя: cid 0 и set_own_avatar.
        store.apply_avatar(Cid(0), [0xC3; 16], vec![7, 7]);
        assert_eq!(store.own_avatar(), Some(&[7, 7][..]));

        // Нулевой id = аватарки нет: снимается, не кэшируется.
        store.apply_avatar(Cid(1), [0; 16], vec![1]);
        assert!(store.avatar(Cid(1)).is_none());
    }
}
