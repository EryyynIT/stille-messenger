//! E2E против реального демона shopot-core: два $HOME, два процесса
//! `shopot serve`, UdsLink с обеих сторон — инвайт → сообщение → доставка.
//!
//! Гейт: запускается только при заданном SHOPOT_BIN (путь к бинарнику
//! ядра); в CI без него тест молча пропускается. Запуск локально:
//!   SHOPOT_BIN=core/zig/zig-out/bin/shopot-zig cargo test --test e2e_daemon

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use stille_messenger::core_link::api::Status;
use stille_messenger::core_link::uds::UdsLink;
use stille_messenger::core_link::{
    CallSignal, CoreLink, LinkError, LinkEvent, OkReply, Reply, Request,
};

struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// Процесс без дома (релей): Drop убивает даже при panic в тесте.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(tag: &str) -> Daemon {
    let bin = std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .expect("SHOPOT_BIN задан (гейт теста)");
    let home =
        std::env::temp_dir().join(format!("stille-messenger-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("дом демона создан");
    // serve требует личность: у свежего $HOME сначала init.
    let init = Command::new(&bin)
        .arg("init")
        .env("HOME", &home)
        .output()
        .expect("init выполнен");
    assert!(
        init.status.success(),
        "init не удался: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let child = Command::new(&bin)
        .arg("serve")
        .env("HOME", &home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("демон запущен");
    Daemon(child, home)
}

async fn wait_socket(home: &std::path::Path) -> PathBuf {
    let sock = home.join(".stille-messenger/api.sock");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if sock.exists() {
            return sock;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("демон не поднял {sock:?} за 15 с");
}

/// Ждать событие, удовлетворяющее предикат; остальное копится в буфер.
async fn wait_event(
    stream: &mut (impl futures_core::Stream<Item = LinkEvent> + Unpin),
    timeout: Duration,
    pred: impl Fn(&LinkEvent) -> bool,
) -> LinkEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            panic!("событие не пришло за {timeout:?}");
        }
        match tokio::time::timeout(left, stream.next()).await {
            Ok(Some(event)) if pred(&event) => return event,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("поток событий закрылся"),
            Err(_) => panic!("таймаут ожидания события"),
        }
    }
}

/// Запрос с повтором при Disconnected: под параллельной нагрузкой
/// e2e-сьюта первый запрос к свежеподнятому демону иногда прилетает
/// до того, как линк восстановил соединение. Ретраи только на обрыв;
/// машинные статусы ядра возвращаются как есть.
async fn execute_retry(link: &UdsLink, request: Request) -> Reply {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match CoreLink::execute(link, request.clone()).await {
            Err(LinkError::Disconnected) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            other => return other,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_message_delivery_two_daemons() {
    let Ok(bin) = std::env::var("STILLE_MESSENGER_BIN").or_else(|_| std::env::var("SHOPOT_BIN"))
    else {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    };
    let _ = bin; // читается в spawn_daemon

    let a = spawn_daemon("a");
    let b = spawn_daemon("b");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;

    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));

    let events_a = CoreLink::events(std::sync::Arc::clone(&link_a));
    let mut events_a = Box::pin(events_a);
    let events_b = CoreLink::events(std::sync::Arc::clone(&link_b));
    let mut events_b = Box::pin(events_b);

    // Сигнал готовности: Resync = супервизор подключился к демону.
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // Оба демона живы: список контактов пуст.
    let reply = CoreLink::execute(&*link_a, Request::Contacts).await;
    assert!(
        matches!(&reply, Ok(OkReply::Contacts(contacts)) if contacts.is_empty()),
        "у свежего демона контактов нет: {reply:?}"
    );

    // A выдаёт инвайт-код, B гасит его — контакт появляется у обоих.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    assert!(
        code.starts_with("stille://i/")
            || code.starts_with("shopot1.")
            || code.starts_with("shopot://i/"),
        "код: {code}"
    );

    let cid_b = match CoreLink::execute(&*link_b, Request::Add { code: code.clone() }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };

    // A видит, что держатель кода подключился.
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_a, name } = joined else {
        unreachable!()
    };

    // Сессия с B поднята (presence online) — иначе отправка уйдёт в outbox.
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // B пишет A: сообщение доходит, ACK возвращается B.
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "привет из e2e".into(),
            entities: Vec::new(),
        },
    )
    .await;
    let Ok(OkReply::Sent { msg_id, queued }) = reply else {
        panic!("SEND_MESSAGE не удался: {reply:?}");
    };
    assert!(!queued, "при живой сессии очередь не нужна");

    let received = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;
    let LinkEvent::MessageReceived {
        cid,
        name: sender_name,
        text,
        msg_id: received_id,
        ..
    } = received
    else {
        unreachable!()
    };
    assert_eq!(cid, cid_a);
    assert_eq!(text, "привет из e2e");
    assert_eq!(received_id, msg_id, "msg_id ядра сквозной");
    assert_eq!(sender_name, name, "имя B согласовано с ContactJoined");

    // ACK доходит до отправителя: статус «доставлено».
    wait_event(
        &mut events_b,
        Duration::from_secs(20),
        |e| matches!(e, LinkEvent::MessageDelivered { msg_id: m, .. } if *m == msg_id),
    )
    .await;

    // S2: блок/разблок на реальном ядре. Заблокированному отправка
    // отклоняется CONTACT_BLOCKED, после разблокировки снова уходит.
    assert_eq!(
        CoreLink::execute(&*link_b, Request::Block { cid: cid_b }).await,
        Ok(OkReply::Done),
        "BLOCK_CONTACT"
    );
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "не уйдёт".into(),
            entities: Vec::new(),
        },
    )
    .await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::ContactBlocked)));
    assert_eq!(
        CoreLink::execute(&*link_b, Request::Unblock { cid: cid_b }).await,
        Ok(OkReply::Done),
        "UNBLOCK_CONTACT"
    );
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "снова уходит".into(),
            entities: Vec::new(),
        },
    )
    .await;
    assert!(matches!(reply, Ok(OkReply::Sent { .. })), "{reply:?}");
}

/// WAN-путь через relay: код с адресом релея, pairing по marker-у,
/// WAN-путь напрямую по адресу: код с host:port держателя, ядро
/// дозванивается само (CORE-API.md §4.2). Тот же сценарий, что у двух
/// разработчиков в разных городах с пробросом порта на роутере одного из них.
#[tokio::test(flavor = "multi_thread")]
async fn addr_invite_message_two_daemons() {
    let Ok(bin) = std::env::var("STILLE_MESSENGER_BIN").or_else(|_| std::env::var("SHOPOT_BIN"))
    else {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    };

    // A пишет stderr в файл: ядро логирует выбранный порт слушателя.
    let home_a = std::env::temp_dir().join(format!("shopot-e2e-addr-a-{}", std::process::id()));
    std::fs::create_dir_all(&home_a).expect("дом A создан");
    let init = Command::new(&bin)
        .arg("init")
        .env("HOME", &home_a)
        .output()
        .expect("init A выполнен");
    assert!(init.status.success(), "init A не удался");
    let log_path = home_a.join("serve.log");
    let log = std::fs::File::create(&log_path).expect("лог A создан");
    let child_a = Command::new(&bin)
        .arg("serve")
        .env("HOME", &home_a)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .expect("демон A запущен");
    let a = Daemon(child_a, home_a);

    let b = spawn_daemon("addr-b");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;

    // Порт A из лога: «авто-режим: слушаю 0.0.0.0:{port}».
    let deadline = Instant::now() + Duration::from_secs(10);
    let port_a = loop {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if let Some(pos) = text.find("слушаю 0.0.0.0:") {
            let digits: String = text[pos + "слушаю 0.0.0.0:".len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(port) = digits.parse::<u16>() {
                break port;
            }
        }
        assert!(
            Instant::now() < deadline,
            "порт A не появился в логе за 10 с"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));

    let events_a = CoreLink::events(std::sync::Arc::clone(&link_a));
    let mut events_a = Box::pin(events_a);
    let events_b = CoreLink::events(std::sync::Arc::clone(&link_b));
    let mut events_b = Box::pin(events_b);

    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // A выдаёт код со своим адресом: B дозвонится напрямую, минуя LAN-поиск.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: format!("127.0.0.1:{port_a}"),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE с адресом не удался: {other:?}"),
    };

    let cid_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE по адресному коду не удался: {other:?}"),
    };

    wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // B пишет A: сообщение доходит напрямую, ACK возвращается.
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "привет по адресному коду".into(),
            entities: Vec::new(),
        },
    )
    .await;
    let Ok(OkReply::Sent { msg_id, queued }) = reply else {
        panic!("SEND_MESSAGE по адресному коду не удался: {reply:?}");
    };
    assert!(!queued, "при живой сессии очередь не нужна");

    wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;
    wait_event(
        &mut events_b,
        Duration::from_secs(20),
        |e| matches!(e, LinkEvent::MessageDelivered { msg_id: m, .. } if *m == msg_id),
    )
    .await;
}

/// S6: критерий приёмки «перезапуск GUI и демона — лента восстанавливается».
/// Переписка двух демонов, kill+restart демона A, GET_HISTORY отдаёт
/// переписку из зашифрованного лога с теми же msg_id и флагами.
#[tokio::test(flavor = "multi_thread")]
async fn history_survives_daemon_restart() {
    let Ok(bin) = std::env::var("STILLE_MESSENGER_BIN").or_else(|_| std::env::var("SHOPOT_BIN"))
    else {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    };

    let mut a = spawn_daemon("hist-a");
    let b = spawn_daemon("hist-b");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;

    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a.clone(),
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let events_a = CoreLink::events(std::sync::Arc::clone(&link_a));
    let mut events_a = Box::pin(events_a);
    let events_b = CoreLink::events(std::sync::Arc::clone(&link_b));
    let mut events_b = Box::pin(events_b);
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // Знакомство: A выдаёт LAN-код, B гасит.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_a, .. } = joined else {
        unreachable!()
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // Переписка: B пишет дважды, A отвечает.
    for text in ["первое до перезапуска", "второе до перезапуска"]
    {
        let reply = CoreLink::execute(
            &*link_b,
            Request::SendMsg {
                cid: cid_b,
                text: text.into(),
                entities: Vec::new(),
            },
        )
        .await;
        assert!(matches!(reply, Ok(OkReply::Sent { .. })), "{reply:?}");
        wait_event(
            &mut events_a,
            Duration::from_secs(20),
            |e| matches!(e, LinkEvent::MessageReceived { text: t, .. } if t == text),
        )
        .await;
    }
    let reply = CoreLink::execute(
        &*link_a,
        Request::SendMsg {
            cid: cid_a,
            text: "ответ A".into(),
            entities: Vec::new(),
        },
    )
    .await;
    assert!(matches!(reply, Ok(OkReply::Sent { .. })), "{reply:?}");
    wait_event(
        &mut events_b,
        Duration::from_secs(20),
        |e| matches!(e, LinkEvent::MessageReceived { text: t, .. } if t == "ответ A"),
    )
    .await;

    // Перезапуск демона A: тот же $HOME, свежий serve, свежий линк.
    // Drop у Daemon добьёт уже мёртвый child — повторный kill безвреден.
    let _ = a.0.kill();
    let _ = a.0.wait();
    let restarted = Proc(
        Command::new(&bin)
            .arg("serve")
            .env("HOME", &a.1)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("демон A перезапущен"),
    );
    let link_a2 = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let events_a2 = CoreLink::events(std::sync::Arc::clone(&link_a2));
    let mut events_a2 = Box::pin(events_a2);
    wait_event(&mut events_a2, Duration::from_secs(15), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // История у A: обе реплики B и свой ответ, порядок старые → новые.
    let reply = CoreLink::execute(
        &*link_a2,
        Request::History {
            cid: cid_a,
            limit: 50,
        },
    )
    .await;
    let Ok(OkReply::History { total, records }) = reply else {
        panic!("GET_HISTORY после перезапуска не удался: {reply:?}");
    };
    assert_eq!(total, 3, "три записи в логе демона A");
    let bodies: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        bodies,
        ["первое до перезапуска", "второе до перезапуска", "ответ A"],
        "порядок старые → новые"
    );
    assert!(
        !records[0].mine && !records[1].mine && records[2].mine,
        "флаги mine согласованы с направлением"
    );

    // История у B: его лог зеркалит переписку.
    let reply = CoreLink::execute(
        &*link_b,
        Request::History {
            cid: cid_b,
            limit: 50,
        },
    )
    .await;
    let Ok(OkReply::History { total, records }) = reply else {
        panic!("GET_HISTORY у B не удался: {reply:?}");
    };
    assert_eq!(total, 3, "у B три записи: {records:?}");
    assert!(
        records[0].mine && records[1].mine && !records[2].mine,
        "у B свои две реплики, чужой ответ последним: {records:?}"
    );
    assert!(
        records.iter().all(|r| !r.mine || r.delivered),
        "свои сообщения подтверждены ACK"
    );

    drop(restarted);
}

/// WAN-путь через relay: код с адресом релея, pairing по marker-у.
/// Стабилизирован ядром 83ea91e (фикс мёртвых сессий f060116 + fake-TLS v2
/// ea2528f): 5/5 прогонов зелёные; ранее нестабилен на acffb0e (~17%,
/// CORE_FEEDBACK.md §5 — закрыт). Входит в гейт scripts/e2e.sh.
#[tokio::test(flavor = "multi_thread")]
async fn relay_invite_message_two_daemons() {
    let Ok(bin) = std::env::var("STILLE_MESSENGER_BIN").or_else(|_| std::env::var("SHOPOT_BIN"))
    else {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    };

    // Релей не требует личности и слушает TCP: порт вне диапазона демонов.
    let addr = "127.0.0.1:17890";
    let relay = Proc(
        Command::new(&bin)
            .arg("relay")
            .arg(addr)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("релей запущен"),
    );

    // Ждём, пока релей начнёт принимать соединения.
    let probe = std::net::SocketAddr::from(([127, 0, 0, 1], 17890));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpStream::connect_timeout(&probe, Duration::from_millis(200)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "релей не поднял {addr} за 10 с");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let a = spawn_daemon("relay-a");
    let b = spawn_daemon("relay-b");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;

    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));

    let events_a = CoreLink::events(std::sync::Arc::clone(&link_a));
    let mut events_a = Box::pin(events_a);
    let events_b = CoreLink::events(std::sync::Arc::clone(&link_b));
    let mut events_b = Box::pin(events_b);

    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // A выдаёт relay-код: соединение пойдёт через релей, IP A скрыт.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: true,
            addr: addr.to_owned(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE relay не удался: {other:?}"),
    };

    // B гасит код; ядро дозванивается через релей само.
    let cid_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE по relay-коду не удался: {other:?}"),
    };

    // Pairing по marker-у: сессия поднимается через релей.
    wait_event(&mut events_a, Duration::from_secs(30), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(30), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // B пишет A: сообщение уходит через релей, ACK возвращается.
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "привет через релей".into(),
            entities: Vec::new(),
        },
    )
    .await;
    let Ok(OkReply::Sent { msg_id, queued }) = reply else {
        panic!("SEND_MESSAGE через релей не удался: {reply:?}");
    };
    assert!(!queued, "через релей сессия живая, очередь не нужна");

    wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;
    wait_event(
        &mut events_b,
        Duration::from_secs(20),
        |e| matches!(e, LinkEvent::MessageDelivered { msg_id: m, .. } if *m == msg_id),
    )
    .await;

    drop(relay);
}

/// S11: поиск по истории (API 2.1+, SEARCH_MESSAGE/GLOBAL_SEARCH_MESSAGE)
/// против реального демона. Ядра < 2.1 метод не знают — тест честно
/// пропускается по зонду GET_VERSION (обратная совместимость гейта).
#[tokio::test(flavor = "multi_thread")]
async fn search_finds_messages_two_daemons() {
    let Ok(bin) = std::env::var("STILLE_MESSENGER_BIN").or_else(|_| std::env::var("SHOPOT_BIN"))
    else {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    };
    let _ = bin;

    let a = spawn_daemon("sa");
    let b = spawn_daemon("sb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;

    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));

    let events_a = CoreLink::events(std::sync::Arc::clone(&link_a));
    let mut events_a = Box::pin(events_a);
    let events_b = CoreLink::events(std::sync::Arc::clone(&link_b));
    let mut events_b = Box::pin(events_b);
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // Гейт: поиск появился в API 2.1; моложе — тест не о чём говорить.
    let reply = CoreLink::execute(&*link_a, Request::Version).await;
    let Ok(OkReply::Version { major, minor, .. }) = reply else {
        panic!("GET_VERSION не удался: {reply:?}")
    };
    if (major, minor) < (2, 1) {
        eprintln!("ядро {major}.{minor} < 2.1 — поиск не поддержан, пропуск");
        return;
    }

    // Знакомство по LAN-коду.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_a, .. } = joined else {
        unreachable!()
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // B пишет A два сообщения с маркером и одно без.
    for text in [
        "маркер: первая записка",
        "пустое место без пометки",
        "маркер: вторая записка",
    ] {
        let reply = CoreLink::execute(
            &*link_b,
            Request::SendMsg {
                cid: cid_b,
                text: text.into(),
                entities: Vec::new(),
            },
        )
        .await;
        assert!(matches!(reply, Ok(OkReply::Sent { .. })), "{reply:?}");
        wait_event(
            &mut events_a,
            Duration::from_secs(20),
            |e| matches!(e, LinkEvent::MessageReceived { text: t, .. } if t == text),
        )
        .await;
    }

    // Чатовый поиск у A: два совпадения, новое первым (§4.3).
    let reply = CoreLink::execute(
        &*link_a,
        Request::Search {
            cid: Some(cid_a),
            query: "маркер".into(),
            limit: 0,
            offset: 0,
        },
    )
    .await;
    let Ok(OkReply::Search { total, hits }) = reply else {
        panic!("SEARCH_MESSAGE не удался: {reply:?}")
    };
    assert_eq!(total, 2, "ровно два совпадения");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].body, "маркер: вторая записка", "новые первыми");
    assert_eq!(hits[1].body, "маркер: первая записка");
    assert!(
        hits.iter().all(|h| h.cid == cid_a && !h.mine && !h.fuzzy),
        "чужие точные совпадения, cid восстановлен из запроса: {hits:?}"
    );

    // Пагинация: вторая страница того же запроса.
    let reply = CoreLink::execute(
        &*link_a,
        Request::Search {
            cid: Some(cid_a),
            query: "маркер".into(),
            limit: 1,
            offset: 1,
        },
    )
    .await;
    let Ok(OkReply::Search { total, hits }) = reply else {
        panic!("SEARCH_MESSAGE страница не удалась: {reply:?}")
    };
    assert_eq!(total, 2);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].body, "маркер: первая записка");

    // Глобальный поиск: те же записи с cid в каждой.
    let reply = CoreLink::execute(
        &*link_a,
        Request::Search {
            cid: None,
            query: "записка".into(),
            limit: 0,
            offset: 0,
        },
    )
    .await;
    let Ok(OkReply::Search { total, hits }) = reply else {
        panic!("GLOBAL_SEARCH_MESSAGE не удался: {reply:?}")
    };
    assert_eq!(total, 2);
    assert!(
        hits.iter().all(|h| h.cid == cid_a),
        "глобальный ответ несёт cid: {hits:?}"
    );

    // Регистр не важен, промах — честный ноль.
    let reply = CoreLink::execute(
        &*link_a,
        Request::Search {
            cid: Some(cid_a),
            query: "МАРКЕР".into(),
            limit: 0,
            offset: 0,
        },
    )
    .await;
    assert!(
        matches!(&reply, Ok(OkReply::Search { total: 2, .. })),
        "регистронезависимо: {reply:?}"
    );
    let reply = CoreLink::execute(
        &*link_a,
        Request::Search {
            cid: Some(cid_a),
            query: "неттакогослова".into(),
            limit: 0,
            offset: 0,
        },
    )
    .await;
    assert!(
        matches!(&reply, Ok(OkReply::Search { total: 0, hits }) if hits.is_empty()),
        "промах — пусто: {reply:?}"
    );
}

/// Закрепы (S12, API 2.2+): закреп чата и сообщения живут в БД ядра
/// и переживают рестарт демона (§4.8). Два демона, LAN-знакомство.
#[tokio::test]
async fn pins_survive_daemon_restart() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let mut a = spawn_daemon("pa");
    let b = spawn_daemon("pb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // Гейт: закрепы появились в API 2.2; моложе — тест не о чём говорить.
    let reply = CoreLink::execute(&*link_a, Request::Version).await;
    let Ok(OkReply::Version { major, minor, .. }) = reply else {
        panic!("GET_VERSION не удался: {reply:?}")
    };
    if (major, minor) < (2, 2) {
        eprintln!("ядро {major}.{minor} < 2.2 — закрепы не поддержаны, пропуск");
        return;
    }

    // Знакомство по LAN-коду.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_a, .. } = joined else {
        unreachable!()
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // B пишет A; A закрепляет чат и это сообщение.
    let reply = CoreLink::execute(
        &*link_b,
        Request::SendMsg {
            cid: cid_b,
            text: "закрепи меня".into(),
            entities: Vec::new(),
        },
    )
    .await;
    assert!(matches!(reply, Ok(OkReply::Sent { .. })), "{reply:?}");
    let received = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;
    let LinkEvent::MessageReceived { msg_id, .. } = received else {
        unreachable!()
    };

    let reply = CoreLink::execute(&*link_a, Request::PinChat { cid: cid_a }).await;
    assert_eq!(reply, Ok(OkReply::Done), "PIN_CHAT: {reply:?}");
    let reply = CoreLink::execute(&*link_a, Request::PinMessage { cid: cid_a, msg_id }).await;
    assert_eq!(reply, Ok(OkReply::Done), "PIN_MESSAGE: {reply:?}");

    // До рестарта: чат закреплён, сообщение в списке, тело по GET_MESSAGE.
    let reply = CoreLink::execute(&*link_a, Request::PinnedChat).await;
    assert_eq!(reply, Ok(OkReply::PinnedChat { cid: Some(cid_a) }));
    let reply = CoreLink::execute(&*link_a, Request::PinnedMessages { cid: cid_a }).await;
    assert_eq!(reply, Ok(OkReply::PinnedMessages { ids: vec![msg_id] }));
    let reply = CoreLink::execute(&*link_a, Request::GetMessage { msg_id }).await;
    assert!(
        matches!(&reply, Ok(OkReply::Message { cid, text, .. })
            if *cid == cid_a && text == "закрепи меня"),
        "GET_MESSAGE: {reply:?}"
    );

    // Рестарт демона A: тот же $HOME (та же БД), новый процесс serve.
    drop(events_a);
    drop(link_a);
    a.0.kill().expect("демон убит");
    let _ = a.0.wait();
    let bin = std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .expect("SHOPOT_BIN задан");
    a.0 = Command::new(&bin)
        .arg("serve")
        .env("HOME", &a.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("демон перезапущен");
    // Старый api.sock может остаться лежать: ждём, пока новый отвечает.
    let sock_a = wait_socket(&a.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // После рестарта закрепы на месте (§4.8: «переживает рестарт»).
    let reply = CoreLink::execute(&*link_a, Request::PinnedChat).await;
    assert_eq!(
        reply,
        Ok(OkReply::PinnedChat { cid: Some(cid_a) }),
        "закреп чата пережил рестарт: {reply:?}"
    );
    let reply = CoreLink::execute(&*link_a, Request::PinnedMessages { cid: cid_a }).await;
    assert_eq!(
        reply,
        Ok(OkReply::PinnedMessages { ids: vec![msg_id] }),
        "закреп сообщения пережил рестарт: {reply:?}"
    );

    // Откреп: чат и сообщение снимаются, повтор идемпотентен.
    let reply = CoreLink::execute(&*link_a, Request::UnpinChat).await;
    assert_eq!(reply, Ok(OkReply::Done));
    let reply = CoreLink::execute(&*link_a, Request::PinnedChat).await;
    assert_eq!(reply, Ok(OkReply::PinnedChat { cid: None }));
    for _ in 0..2 {
        let reply = CoreLink::execute(&*link_a, Request::UnpinMessage { cid: cid_a, msg_id }).await;
        assert_eq!(reply, Ok(OkReply::Done), "UNPIN идемпотентен: {reply:?}");
    }
    let reply = CoreLink::execute(&*link_a, Request::PinnedMessages { cid: cid_a }).await;
    assert_eq!(reply, Ok(OkReply::PinnedMessages { ids: vec![] }));
}

/// Визитки и удаление чата (S13, API 2.2+): A знакомится с B и C,
/// шлёт B свою визитку и визитку C (события CONTACT_CARD у B), затем
/// удаляет чат с B — контакт, история и закреп исчезают и не
/// воскресают после рестарта демона (§4.8). У B своя копия остаётся.
#[tokio::test]
async fn cards_and_chat_delete_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let mut a = spawn_daemon("ca");
    let b = spawn_daemon("cb");
    let c = spawn_daemon("cc");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let sock_c = wait_socket(&c.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let link_c = std::sync::Arc::new(UdsLink::with_config(
        sock_c,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    let mut events_c = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_c)));
    for events in [&mut events_a, &mut events_b, &mut events_c] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    // Гейт: визитки и удаление — API 2.2; моложе — тест не о чём говорить.
    let reply = CoreLink::execute(&*link_a, Request::Version).await;
    let Ok(OkReply::Version { major, minor, .. }) = reply else {
        panic!("GET_VERSION не удался: {reply:?}")
    };
    if (major, minor) < (2, 2) {
        eprintln!("ядро {major}.{minor} < 2.2 — визитки/удаление не поддержаны, пропуск");
        return;
    }

    // Знакомство A↔B и A↔C по LAN-кодам, по очереди.
    let mut cid_at_a = Vec::new();
    let mut cid_of_a_at = Vec::new();
    for (link_peer, events_peer) in [(&link_b, &mut events_b), (&link_c, &mut events_c)] {
        let code = match execute_retry(
            &link_a,
            Request::Invite {
                relay: false,
                addr: String::new(),
            },
        )
        .await
        {
            Ok(OkReply::Code(code)) => code,
            other => panic!("CREATE_INVITE не удался: {other:?}"),
        };
        let cid_peer = match execute_retry(link_peer, Request::Add { code }).await {
            Ok(OkReply::Added { cid }) => cid,
            other => panic!("ACCEPT_INVITE не удался: {other:?}"),
        };
        let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
            matches!(e, LinkEvent::ContactJoined { .. })
        })
        .await;
        let LinkEvent::ContactJoined { cid, .. } = joined else {
            unreachable!()
        };
        cid_at_a.push(cid);
        cid_of_a_at.push(cid_peer);
        wait_event(events_peer, Duration::from_secs(20), |e| {
            matches!(e, LinkEvent::Presence { online: true, .. })
        })
        .await;
    }
    let (cid_b, cid_c) = (cid_at_a[0], cid_at_a[1]);
    let cid_a_at_b = cid_of_a_at[0];

    // A шлёт B свою визитку (contact = None). Имя в карточке — имя
    // профиля A; оно не задано (SET_NAME не вызывался), поэтому пусто —
    // квирк ядра: «shopot» у B — дефолт инвайта, а не профиль.
    let reply = execute_retry(
        &link_a,
        Request::SendContact {
            to: cid_b,
            contact: None,
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "SEND_CONTACT своей: {reply:?}");
    let card = wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactCard { .. })
    })
    .await;
    let LinkEvent::ContactCard {
        cid: card_from,
        name: card_name,
        bundle: bundle_a,
        ..
    } = card
    else {
        unreachable!()
    };
    assert_eq!(card_from, cid_a_at_b, "cid события — отправитель визитки");
    assert!(
        card_name.is_empty() || card_name == "shopot",
        "имя профиля не задано: пусто (83ea91e) или дефолт ядра: {card_name:?}"
    );

    // A пересылает B визитку C: другой человек — другой bundle.
    let reply = execute_retry(
        &link_a,
        Request::SendContact {
            to: cid_b,
            contact: Some(cid_c),
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "SEND_CONTACT чужой: {reply:?}");
    let card = wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactCard { .. })
    })
    .await;
    let LinkEvent::ContactCard {
        cid: card_from,
        bundle: bundle_c,
        ..
    } = card
    else {
        unreachable!()
    };
    assert_eq!(card_from, cid_a_at_b);
    assert_ne!(bundle_c, bundle_a, "визитка C — не визитка A");

    // Ошибки честные: неизвестный получатель и неизвестная карточка.
    let reply = CoreLink::execute(
        &*link_a,
        Request::SendContact {
            to: stille_messenger::model::cid::Cid(99),
            contact: None,
        },
    )
    .await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));

    // A закрепляет чат с B, затем удаляет его: у ядра чистятся история,
    // закреп и сам контакт (§4.8).
    let reply = CoreLink::execute(&*link_a, Request::PinChat { cid: cid_b }).await;
    assert_eq!(reply, Ok(OkReply::Done), "PIN_CHAT: {reply:?}");
    let reply = CoreLink::execute(&*link_a, Request::DeleteChat { cid: cid_b }).await;
    assert_eq!(reply, Ok(OkReply::Done), "DELETE_CHAT: {reply:?}");

    let reply = execute_retry(&link_a, Request::Contacts).await;
    let Ok(OkReply::Contacts(contacts_a)) = reply else {
        panic!("LIST_CONTACTS у A: {reply:?}")
    };
    assert!(
        contacts_a.iter().all(|x| x.cid != cid_b),
        "удалённый контакт не возвращается: {contacts_a:?}"
    );
    assert!(
        contacts_a.iter().any(|x| x.cid == cid_c),
        "чат с C не пострадал"
    );
    let reply = CoreLink::execute(&*link_a, Request::PinnedChat).await;
    assert_eq!(
        reply,
        Ok(OkReply::PinnedChat { cid: None }),
        "закреп удалённого чата сброшен"
    );
    let reply = CoreLink::execute(
        &*link_a,
        Request::History {
            cid: cid_b,
            limit: 0,
        },
    )
    .await;
    assert_eq!(
        reply,
        Err(LinkError::Kernel(Status::NoContact)),
        "история удалённого недоступна: {reply:?}"
    );
    let reply = CoreLink::execute(
        &*link_a,
        Request::SendMsg {
            cid: cid_b,
            text: "уже некому".into(),
            entities: Vec::new(),
        },
    )
    .await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::NoContact)));

    // У B своя копия осталась: удаление — локальное действие A.
    let reply = execute_retry(&link_b, Request::Contacts).await;
    let Ok(OkReply::Contacts(contacts_b)) = reply else {
        panic!("LIST_CONTACTS у B: {reply:?}")
    };
    assert!(
        contacts_b.iter().any(|x| x.cid == cid_a_at_b),
        "у B контакт с A на месте"
    );

    // Рестарт демона A: удаление переживает рестарт (слот-надгробие).
    drop(events_a);
    drop(link_a);
    a.0.kill().expect("демон убит");
    let _ = a.0.wait();
    let bin = std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .expect("SHOPOT_BIN задан");
    a.0 = Command::new(&bin)
        .arg("serve")
        .env("HOME", &a.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("демон перезапущен");
    let sock_a = wait_socket(&a.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    let reply = execute_retry(&link_a, Request::Contacts).await;
    let Ok(OkReply::Contacts(contacts_a)) = reply else {
        panic!("LIST_CONTACTS после рестарта: {reply:?}")
    };
    assert!(
        contacts_a.iter().all(|x| x.cid != cid_b),
        "удалённый не воскрес после рестарта: {contacts_a:?}"
    );
    assert!(contacts_a.iter().any(|x| x.cid == cid_c));
}

/// Имя профиля (S13.1, SET_NAME): A задаёт имя и шлёт B свою визитку —
/// карточка у B приходит с этим именем, а не безымянной (квирк ядра из
/// CORE_FEEDBACK §7 обходится именно так).
#[tokio::test]
async fn own_card_carries_set_name() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("na");
    let b = spawn_daemon("nb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    wait_event(&mut events_a, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;
    wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(e, LinkEvent::Resync)
    })
    .await;

    // Визитка — API 2.2; моложе — тест не о чём говорить.
    let reply = CoreLink::execute(&*link_a, Request::Version).await;
    let Ok(OkReply::Version { major, minor, .. }) = reply else {
        panic!("GET_VERSION не удался: {reply:?}")
    };
    if (major, minor) < (2, 2) {
        eprintln!("ядро {major}.{minor} < 2.2 — визитки не поддержаны, пропуск");
        return;
    }

    // Знакомство A↔B по LAN-коду.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // A задаёт имя (кириллица — str16 UTF-8) и шлёт свою визитку.
    let reply = execute_retry(
        &link_a,
        Request::SetName {
            name: "Михаил e2e".into(),
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "SET_NAME: {reply:?}");
    let reply = execute_retry(
        &link_a,
        Request::SendContact {
            to: cid_b,
            contact: None,
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "SEND_CONTACT своей: {reply:?}");

    let card = wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactCard { .. })
    })
    .await;
    let LinkEvent::ContactCard {
        cid: card_from,
        name: card_name,
        ..
    } = card
    else {
        unreachable!()
    };
    assert_eq!(card_from, cid_a_at_b, "cid события — отправитель визитки");
    assert_eq!(
        card_name, "Михаил e2e",
        "своя визитка несёт заданное имя, а не дефолт ядра"
    );
}

/// Два демона: звонок A → B (S7). START_CALL → offer у обоих, ANSWER
/// у B → accept у обоих, END у A → end у обоих; стадии по call_id.
/// Звонки есть с API 1.0 — гейта по версии нет; нужна живая сессия.
#[tokio::test]
async fn call_offer_answer_end_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("la");
    let b = spawn_daemon("lb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    // Знакомство A↔B по LAN-коду, ждём живую сессию (без неё NO_SESSION).
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;
    wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Presence { online: true, .. })
    })
    .await;

    // A звонит B: ответ несёт call_id, оба получают offer.
    let reply = execute_retry(&link_a, Request::StartCall { cid: cid_b }).await;
    let Ok(OkReply::CallStarted { call_id }) = reply else {
        panic!("START_CALL не удался: {reply:?}")
    };
    let offer_at_b = wait_event(&mut events_b, Duration::from_secs(10), |e| {
        matches!(
            e,
            LinkEvent::CallState {
                state: CallSignal::Offer,
                ..
            }
        )
    })
    .await;
    let LinkEvent::CallState {
        cid: offer_cid,
        call_id: offer_id,
        ..
    } = offer_at_b
    else {
        unreachable!()
    };
    assert_eq!(offer_cid, cid_a_at_b, "offer от A");
    assert_eq!(offer_id, call_id, "call_id стабилен между сторонами");

    // B принимает: оба получают accept (у B ядро поднимает звук —
    // fifo живут в его доме, GUI-концов в тесте нет, ядро читает пустое
    // молчание и не падает).
    let reply = execute_retry(&link_b, Request::AnswerCall).await;
    assert_eq!(reply, Ok(OkReply::Done), "ANSWER_CALL: {reply:?}");
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(
                e,
                LinkEvent::CallState {
                    state: CallSignal::Accept,
                    ..
                }
            )
        })
        .await;
    }

    // A завершает: оба получают end, повторный END — NO_CALL.
    let reply = execute_retry(&link_a, Request::EndCall).await;
    assert_eq!(reply, Ok(OkReply::Done), "END_CALL: {reply:?}");
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(
                e,
                LinkEvent::CallState {
                    state: CallSignal::End,
                    ..
                }
            )
        })
        .await;
    }
    let reply = CoreLink::execute(&*link_a, Request::EndCall).await;
    assert_eq!(
        reply,
        Err(LinkError::Kernel(Status::NoCall)),
        "повторный END_CALL — NO_CALL: {reply:?}"
    );
}

/// Два демона: разметка доезжает (S14). A шлёт «**жирно**» как тело +
/// entities; B получает MESSAGE_RECEIVED с теми же entities, история
/// у B их же хранит. Entities — API 2.0+; моложе — пропуск.
#[tokio::test]
async fn formatted_message_entities_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("ea");
    let b = spawn_daemon("eb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    // Гейт: entities — API 2.0+.
    let reply = CoreLink::execute(&*link_a, Request::Version).await;
    let Ok(OkReply::Version { major, .. }) = reply else {
        panic!("GET_VERSION не удался: {reply:?}")
    };
    if major < 2 {
        eprintln!("ядро {major}.x < 2.0 — разметки нет, пропуск");
        return;
    }

    // Знакомство A↔B по LAN-коду.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };

    // Тело «жирно и тайна»: «жирно» bold [0..10], «тайна» spoiler [14..24]
    // (байты UTF-8: кириллица по 2).
    let entities = vec![
        stille_messenger::core_link::Entity::new(
            stille_messenger::core_link::EntityKind::Bold,
            0,
            10,
        ),
        stille_messenger::core_link::Entity::new(
            stille_messenger::core_link::EntityKind::Spoiler,
            14,
            10,
        ),
    ];
    let reply = execute_retry(
        &link_a,
        Request::SendMsg {
            cid: cid_b,
            text: "жирно и тайна".into(),
            entities: entities.clone(),
        },
    )
    .await;
    let Ok(OkReply::Sent { .. }) = reply else {
        panic!("SEND_MESSAGE с entities не удался: {reply:?}")
    };

    // Событие у B несёт те же entities.
    let event = wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;
    let LinkEvent::MessageReceived {
        cid: from,
        text,
        entities: got,
        ..
    } = event
    else {
        unreachable!()
    };
    assert_eq!(from, cid_a_at_b);
    assert_eq!(text, "жирно и тайна");
    assert_eq!(got, entities, "entities доехали без искажений");

    // История у B хранит entities (перечитываем с диска ядра).
    let reply = execute_retry(
        &link_b,
        Request::History {
            cid: cid_a_at_b,
            limit: 10,
        },
    )
    .await;
    let Ok(OkReply::History { records, .. }) = reply else {
        panic!("GET_HISTORY не удался: {reply:?}")
    };
    let record = records.iter().find(|r| r.text == "жирно и тайна");
    assert_eq!(
        record.map(|r| r.entities.as_slice()),
        Some(entities.as_slice()),
        "история хранит разметку"
    );

    // Кривая разметка — BAD_ARGS (правила §4.3 проверяет ядро).
    let reply = CoreLink::execute(
        &*link_a,
        Request::SendMsg {
            cid: cid_b,
            text: "короткий".into(),
            entities: vec![stille_messenger::core_link::Entity::new(
                stille_messenger::core_link::EntityKind::Bold,
                0,
                999,
            )],
        },
    )
    .await;
    assert_eq!(
        reply,
        Err(LinkError::Kernel(Status::BadArgs)),
        "сущность за границей тела — BAD_ARGS: {reply:?}"
    );
}

/// Два демона: реакции (S15). B ставит 👍 на сообщение A — A получает
/// REACTION add; B снимает — A получает remove. Своя реакция ставится
/// с любого ядра (1.0+), гейта по версии нет.
#[tokio::test]
async fn reactions_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("ra");
    let b = spawn_daemon("rb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    // Знакомство A↔B.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };
    // Присутствие обеих сторон: реакция требует живой сессии.
    for (events, cid) in [(&mut events_a, cid_b), (&mut events_b, cid_a_at_b)] {
        wait_event(
            events,
            Duration::from_secs(20),
            |e| matches!(e, LinkEvent::Presence { cid: c, online: true, .. } if *c == cid),
        )
        .await;
    }

    // A шлёт сообщение; у B оно получает msg_id.
    let reply = execute_retry(
        &link_a,
        Request::SendMsg {
            cid: cid_b,
            text: "оцени".into(),
            entities: Vec::new(),
        },
    )
    .await;
    let Ok(OkReply::Sent { msg_id, .. }) = reply else {
        panic!("SEND_MESSAGE не удался: {reply:?}")
    };
    wait_event(&mut events_b, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::MessageReceived { .. })
    })
    .await;

    // B ставит 👍 → A видит событие add с тем же msg_id и кодпоинтом.
    let reply = CoreLink::execute(
        &*link_b,
        Request::SetReaction {
            cid: cid_a_at_b,
            msg_id,
            emoji: '👍',
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "SET_REACTION: {reply:?}");
    let event = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Reaction { add: true, .. })
    })
    .await;
    let LinkEvent::Reaction {
        cid,
        msg_id: got,
        emoji,
        ..
    } = event
    else {
        unreachable!()
    };
    assert_eq!(cid, cid_b);
    assert_eq!(got, msg_id, "реакция адресует то сообщение");
    assert_eq!(emoji, '👍');

    // B снимает → A видит remove.
    let reply = CoreLink::execute(
        &*link_b,
        Request::RemoveReaction {
            cid: cid_a_at_b,
            msg_id,
            emoji: '👍',
        },
    )
    .await;
    assert_eq!(reply, Ok(OkReply::Done), "REMOVE_REACTION: {reply:?}");
    let event = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::Reaction { add: false, .. })
    })
    .await;
    let LinkEvent::Reaction { add, emoji, .. } = event else {
        unreachable!()
    };
    assert!(!add);
    assert_eq!(emoji, '👍');
}

/// Два демона: файл доезжает (S10). A шлёт реальный файл с диска; B
/// получает FILE_RECEIVED, файл лежит в его ~/.stille-messenger/files/<hex> с тем
/// же содержимым; A получает FILE_SENT — честный «отправлен».
#[tokio::test]
async fn file_transfer_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("fa");
    let b = spawn_daemon("fb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    // Знакомство A↔B.
    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };
    // Сессия обеих сторон (файл можно и в outbox, но так быстрее).
    for (events, cid) in [(&mut events_a, cid_b), (&mut events_b, cid_a_at_b)] {
        wait_event(
            events,
            Duration::from_secs(20),
            |e| matches!(e, LinkEvent::Presence { cid: c, online: true, .. } if *c == cid),
        )
        .await;
    }

    // Файл на диске A (демон читает сам — путь его ФС).
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let src = std::env::temp_dir().join(format!("shopot-e2e-file-{}.bin", std::process::id()));
    std::fs::write(&src, &payload).expect("файл записан");

    let reply = CoreLink::execute(
        &*link_a,
        Request::SendFile {
            cid: cid_b,
            path: src.to_string_lossy().into_owned(),
        },
    )
    .await;
    let Ok(OkReply::FileQueued { file_id, size, .. }) = reply else {
        panic!("SEND_FILE не удался: {reply:?}")
    };
    assert_eq!(size as usize, payload.len());

    // B: FILE_RECEIVED + файл на диске с тем же содержимым.
    let event = wait_event(&mut events_b, Duration::from_secs(30), |e| {
        matches!(e, LinkEvent::FileReceived { .. })
    })
    .await;
    let LinkEvent::FileReceived {
        cid,
        file_id: got,
        size: got_size,
        name,
    } = event
    else {
        unreachable!()
    };
    assert_eq!(cid, cid_a_at_b);
    assert_eq!(got, file_id, "file_id = SHA-256[0..16], у обеих один");
    assert_eq!(got_size as usize, payload.len());
    assert_eq!(name, src.file_name().unwrap().to_string_lossy());

    let hex: String = file_id.iter().map(|b| format!("{b:02x}")).collect();
    let stored = b.1.join(".stille-messenger/files").join(&hex);
    let stored_bytes =
        std::fs::read(&stored).unwrap_or_else(|e| panic!("файл у B на диске ({:?}): {e}", stored));
    assert_eq!(stored_bytes, payload, "содержимое доехало без искажений");

    // A: FILE_SENT — все чанки подтверждены.
    let event = wait_event(&mut events_a, Duration::from_secs(30), |e| {
        matches!(e, LinkEvent::FileSent { .. })
    })
    .await;
    let LinkEvent::FileSent { cid, file_id: got } = event else {
        unreachable!()
    };
    assert_eq!(cid, cid_b);
    assert_eq!(got, file_id);

    // Нет файла — честный FILE_NOT_FOUND.
    let reply = CoreLink::execute(
        &*link_a,
        Request::SendFile {
            cid: cid_b,
            path: "/нет/такого.bin".into(),
        },
    )
    .await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::FileNotFound)));

    std::fs::remove_file(&src).ok();
}

/// Два демона: аватарка доезжает (S8б). A ставит фото; B (онлайн)
/// получает AVATAR_RECEIVED с тем же avatar_id и байтами; GET_AVATAR_ID
/// у B для A отдаёт тот же id.
#[tokio::test]
async fn avatar_two_daemons() {
    if std::env::var("STILLE_MESSENGER_BIN")
        .or_else(|_| std::env::var("SHOPOT_BIN"))
        .is_err()
    {
        eprintln!("SHOPOT_BIN не задан — e2e против демона пропущен");
        return;
    }

    let a = spawn_daemon("ava");
    let b = spawn_daemon("avb");
    let sock_a = wait_socket(&a.1).await;
    let sock_b = wait_socket(&b.1).await;
    let link_a = std::sync::Arc::new(UdsLink::with_config(
        sock_a,
        Duration::from_millis(100),
        false,
    ));
    let link_b = std::sync::Arc::new(UdsLink::with_config(
        sock_b,
        Duration::from_millis(100),
        false,
    ));
    let mut events_a = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_a)));
    let mut events_b = Box::pin(CoreLink::events(std::sync::Arc::clone(&link_b)));
    for events in [&mut events_a, &mut events_b] {
        wait_event(events, Duration::from_secs(10), |e| {
            matches!(e, LinkEvent::Resync)
        })
        .await;
    }

    let code = match execute_retry(
        &link_a,
        Request::Invite {
            relay: false,
            addr: String::new(),
        },
    )
    .await
    {
        Ok(OkReply::Code(code)) => code,
        other => panic!("CREATE_INVITE не удался: {other:?}"),
    };
    let cid_a_at_b = match execute_retry(&link_b, Request::Add { code }).await {
        Ok(OkReply::Added { cid }) => cid,
        other => panic!("ACCEPT_INVITE не удался: {other:?}"),
    };
    let joined = wait_event(&mut events_a, Duration::from_secs(20), |e| {
        matches!(e, LinkEvent::ContactJoined { .. })
    })
    .await;
    let LinkEvent::ContactJoined { cid: cid_b, .. } = joined else {
        unreachable!()
    };
    for (events, cid) in [(&mut events_a, cid_b), (&mut events_b, cid_a_at_b)] {
        wait_event(
            events,
            Duration::from_secs(20),
            |e| matches!(e, LinkEvent::Presence { cid: c, online: true, .. } if *c == cid),
        )
        .await;
    }

    // A ставит аватарку (маленький «PNG» — ядро формат не разбирает).
    let data: Vec<u8> = std::iter::once(vec![0x89, 0x50, 0x4E, 0x47])
        .chain(std::iter::once(
            (0..2000u32).map(|i| (i % 251) as u8).collect(),
        ))
        .flatten()
        .collect();
    let reply = CoreLink::execute(&*link_a, Request::SetAvatar { data: data.clone() }).await;
    let Ok(OkReply::AvatarSet { avatar_id, size }) = reply else {
        panic!("SET_AVATAR не удался: {reply:?}")
    };
    assert_eq!(size as usize, data.len());

    // B: AVATAR_RECEIVED с теми же id и байтами.
    let event = wait_event(&mut events_b, Duration::from_secs(30), |e| {
        matches!(e, LinkEvent::AvatarReceived { .. })
    })
    .await;
    let LinkEvent::AvatarReceived {
        cid,
        avatar_id: got_id,
        data: got_data,
    } = event
    else {
        unreachable!()
    };
    assert_eq!(cid, cid_a_at_b);
    assert_eq!(
        got_id, avatar_id,
        "avatar_id — content addressing, совпадает"
    );
    assert_eq!(got_data, data, "байты без искажений");

    // GET_AVATAR_ID у B согласован.
    let reply = CoreLink::execute(&*link_b, Request::GetAvatarId { cid: cid_a_at_b }).await;
    assert_eq!(reply, Ok(OkReply::AvatarId(avatar_id)));

    // Лимиты: пустая и 16001 Б — обе TOO_BIG (нарушение рамки размера
    // ядро отвечает одним кодом).
    let reply = CoreLink::execute(&*link_a, Request::SetAvatar { data: vec![] }).await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::TooBig)));
    let reply = CoreLink::execute(
        &*link_a,
        Request::SetAvatar {
            data: vec![0x89; 16001],
        },
    )
    .await;
    assert_eq!(reply, Err(LinkError::Kernel(Status::TooBig)));
}
