//! UI-тесты на iced_test: headless-эмуляция всего приложения
//! (подписки и задачи — как вживую). Сцена — детерминированная.
//!
//! Все интерактивные сценарии выполняются в одном #[test]: эмуляторы
//! делят общий глобальный tokio-runtime iced, и при параллельном
//! запуске задачи одного эмулятора мешают доставке задач другого.

use iced::futures::channel::mpsc;
use iced_test::emulator::{self, Emulator, Mode};
use iced_test::instruction::{Expectation, Interaction, Key, Keyboard, Mouse, Target};
use iced_test::Instruction;
use shopot_gui::app::{self, Link};
use shopot_gui::core_link::mock::Scene;
use shopot_gui::model::cid::Cid;
use shopot_gui::model::msg::SendState;
use std::time::{Duration, Instant};

type Event<P> = emulator::Event<P>;

/// Качает события эмулятора `ms` миллисекунд реального времени.
fn pump<P: iced_test::program::Program + 'static>(
    emu: &mut Emulator<P>,
    program: &P,
    receiver: &mut mpsc::Receiver<Event<P>>,
    ms: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if let Ok(Event::Action(action)) = receiver.try_recv() {
            emu.perform(program, action);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Сливает всё, что уже пришло; возвращает число событий.
fn drain<P: iced_test::program::Program + 'static>(
    emu: &mut Emulator<P>,
    program: &P,
    receiver: &mut mpsc::Receiver<Event<P>>,
) -> usize {
    let mut seen = 0;
    while let Ok(event) = receiver.try_recv() {
        seen += 1;
        match event {
            Event::Action(action) => emu.perform(program, action),
            Event::Ready | Event::Failed(_) => {}
        }
    }
    seen
}

/// Ждёт «тихого периода»: окно, за которое не пришло ни одного события.
/// Гарантирует, что у всех задач, порождённых прошлыми действиями,
/// завершены и продолжения — их Ready/Failed не подделают результат
/// следующей инструкции.
fn wait_quiet<P: iced_test::program::Program + 'static>(
    emu: &mut Emulator<P>,
    program: &P,
    receiver: &mut mpsc::Receiver<Event<P>>,
    max_ms: u64,
) {
    let begin = Instant::now();
    let deadline = begin + Duration::from_millis(max_ms);
    let settled = begin + Duration::from_millis(200);
    while Instant::now() < deadline {
        let seen = drain(emu, program, receiver);
        // Тишина считается честной только после того, как все задачи
        // загрузки успели проявиться: в Immediate-режиме Ready от boot
        // приходит в канал с запаздыванием и без этого окна подделывает
        // результат первой же инструкции.
        if seen == 0 && Instant::now() >= settled {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Выполняет инструкцию и дожидается её результата на канале.
fn run_instruction<P: iced_test::program::Program + 'static>(
    emu: &mut Emulator<P>,
    program: &P,
    receiver: &mut mpsc::Receiver<Event<P>>,
    instruction: Instruction,
) {
    let label = format!("{instruction:?}");
    wait_quiet(emu, program, receiver, 3000);
    emu.run(program, instruction);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match receiver.try_recv() {
            Ok(Event::Action(action)) => emu.perform(program, action),
            Ok(Event::Ready) => return,
            Ok(Event::Failed(_)) => panic!("инструкция не выполнена: {label}"),
            Err(_) => {}
        }
        if Instant::now() > deadline {
            panic!("инструкция не завершилась за 5 с: {label}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn click(text: &str) -> [Instruction; 2] {
    [
        Instruction::Interact(Interaction::Mouse(Mouse::Press {
            button: iced::mouse::Button::Left,
            target: Some(Target::Text(text.to_owned())),
        })),
        Instruction::Interact(Interaction::Mouse(Mouse::Release {
            button: iced::mouse::Button::Left,
            target: Some(Target::Text(text.to_owned())),
        })),
    ]
}

fn right_click(text: &str) -> [Instruction; 2] {
    [
        Instruction::Interact(Interaction::Mouse(Mouse::Press {
            button: iced::mouse::Button::Right,
            target: Some(Target::Text(text.to_owned())),
        })),
        Instruction::Interact(Interaction::Mouse(Mouse::Release {
            button: iced::mouse::Button::Right,
            target: Some(Target::Text(text.to_owned())),
        })),
    ]
}

/// RGBA-кадр → PNG-файл (dev-зависимость png).
fn save_png(shot: iced_test::core::window::Screenshot, path: &std::path::Path) {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, shot.size.width, shot.size.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer
            .write_image_data(&shot.rgba)
            .expect("rgba кадр пишется в PNG");
    }
    std::fs::write(path, png).expect("скриншот сохраняется");
}

/// Финал сценария: при SHOPOT_SHOTS=1 снимок финального состояния в
/// target/shots/<имя>.png (глаза на результат), затем забрать State.
fn finish<P>(mut emu: Emulator<P>, program: &P, name: &str) -> P::State
where
    P: iced_test::program::Program + 'static,
{
    if std::env::var("SHOPOT_SHOTS").as_deref() == Ok("1") {
        let theme = emu.theme(program).expect("приложение задаёт тему");
        let shot = emu.screenshot(program, &theme, 1.0);
        let busy = busy_share(&shot);
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/shots");
        std::fs::create_dir_all(dir).expect("каталог скриншотов создан");
        save_png(shot, &std::path::Path::new(dir).join(format!("{name}.png")));
        println!("   скриншот: {busy:.1}% пикселей не фон — окно не пустое");
        assert!(busy > 1.0, "кадр почти пуст: {busy:.1}% — рендер сломан?");
    }
    emu.into_state().0
}

/// Доля пикселей, отличных от фонового цвета левого верхнего угла
/// (каждый 4-й пиксель — статистике хватает). Машинная замена «посмотреть
/// глазами»: пустое/чёрное окно и потерянная тема ловятся без человека.
fn busy_share(shot: &iced_test::core::window::Screenshot) -> f64 {
    let i = (2 * shot.size.width + 2) as usize * 4;
    let bg = (
        shot.rgba[i],
        shot.rgba[i + 1],
        shot.rgba[i + 2],
        shot.rgba[i + 3],
    );
    let mut total = 0usize;
    let mut busy = 0usize;
    for px in shot.rgba.chunks_exact(16) {
        total += 1;
        if (px[0], px[1], px[2], px[3]) != bg {
            busy += 1;
        }
    }
    busy as f64 / total.max(1) as f64 * 100.0
}

/// Каждый сценарий гоняется в общем #[test] (эмуляторы делят глобальный
/// tokio-runtime iced: при параллельном запуске задачи одного эмулятора
/// мешают доставке задач другого), но под catch_unwind: падение одного
/// сценария не глушит остальные — отчёт всегда по всем, слепых зон нет.
const SCENARIOS: &[(&str, fn())] = &[
    ("contacts→сайдбар", contacts_reply_populates_sidebar),
    ("входящее→непрочитанность", incoming_message_counts_unread),
    (
        "отправка→ACK→автоответ",
        send_flow_shows_delivered_and_autoreply,
    ),
    (
        "история→восстановление ленты",
        history_restores_on_chat_open,
    ),
    (
        "инвайт: выдать и добавить",
        invite_dialog_cycles_issue_and_add,
    ),
    ("инвайт: адрес + relay", invite_with_address_and_relay),
    (
        "ПКМ: блок и карточка",
        right_click_menu_blocks_and_opens_card,
    ),
    ("Esc закрывает оверлей", escape_closes_overlay),
    (
        "настройки: галки и масштаб",
        settings_overlay_toggles_and_closes,
    ),
    ("звонок: оверлей и mute", call_overlay_and_mute),
    ("разметка в ленте", formatted_message_renders),
    ("нейтральные подписи: resize и 120%", neutral_text_matrix),
    ("длинные имена: resize и 120%", long_names_layout_matrix),
];

#[test]
fn smoke_scenarios() {
    // Крюк ui::notify::os_notify: в headless-тестах нет десктопа —
    // входящие из сцены не должны всплывать уведомлениями ОС.
    std::env::set_var("SHOPOT_NO_NOTIFY", "1");

    let mut failed = Vec::new();
    for (name, scenario) in SCENARIOS {
        let began = Instant::now();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(*scenario));
        match outcome {
            Ok(()) => println!("✓ {name} ({:.1?})", began.elapsed()),
            Err(payload) => {
                let reason = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "паника без сообщения".to_owned());
                println!("✗ {name}: {reason}");
                failed.push(*name);
            }
        }
    }
    assert!(failed.is_empty(), "упали сценарии: {failed:?}");
}

/// Код с адресом и relay: ядро дозванивается само, peer IP не видит
/// (CORE-API.md §4.2). Тот же путь, что и у двух разработчиков в разных
/// городах через релей на VPS.
fn invite_with_address_and_relay() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    let [press, release] = click("Пригласить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    // Форма ждёт, код не выдан.
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Код появится после выдачи.".into())),
    );

    // Адрес и relay-галка.
    let [press, release] = click("192.168.1.5:7777 — необязательно");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Typewrite(
            "192.168.1.5:7777".into(),
        ))),
    );
    let [press, release] = click("через посредника — собеседник не увидит ваш IP");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    // Выдача: код отражает relay-режим заглушки.
    let [press, release] = click("Выдать код");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Скопировать код".into())),
    );

    let state = finish(emu, &program, "invite_with_address_and_relay");
    // relay-код не кэшируется как LAN: следующее открытие снова форма.
    assert!(
        state.invite_code.is_none(),
        "relay-код не должен попадать в LAN-кэш"
    );
    // На экране эллипсис случайного кода, поэтому режим сверяем по состоянию.
    if let shopot_gui::ui::Overlay::Invite(view) = &state.overlay {
        assert!(view.relay, "галка relay в форме выданного кода");
        let code = view.code.as_deref().expect("код выдан");
        assert!(
            code.starts_with("shopot1r."),
            "код relay-режима от заглушки: {code}"
        );
        assert!(
            code.ends_with("@192.168.1.5:7777"),
            "адрес попал в код: {code}"
        );
    } else {
        panic!("диалог приглашения должен остаться открытым");
    }
}

/// contacts → сайдбар populated: имя, статус «в сети», счётчик сессий.
fn contacts_reply_populates_sidebar() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    let state = finish(emu, &program, "contacts_reply_populates_sidebar");
    let contact = state.store.contact(Cid(1)).expect("контакт после contacts");
    assert_eq!(contact.name, "Аня");
    assert!(contact.online);
    assert_eq!(state.store.online_count(), 1);
}

/// Входящее при закрытом чате даёт непрочитанность ровно 1.
fn incoming_message_counts_unread() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    pump(&mut emu, &program, &mut receiver, 1000);

    let state = finish(emu, &program, "incoming_message_counts_unread");
    let thread = state.store.thread(Cid(1)).expect("Аня пришла из contacts");
    assert_eq!(
        thread.unread, 1,
        "входящее «пинг» вне открытого чата — бейдж 1"
    );
}

/// Отправка: ACK ядра показывает «доставлено», затем автоответ.
fn send_flow_shows_delivered_and_autoreply() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    pump(&mut emu, &program, &mut receiver, 1000);

    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    // Открытие чата фокусирует композер — печатаем сразу.
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Typewrite("привет".into()))),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Type(Key::Enter))),
    );

    pump(&mut emu, &program, &mut receiver, 1500);

    // Лента показывает доставку (ACK ядра) и автоответ.
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("доставлено".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("принято".into())),
    );

    let state = finish(emu, &program, "send_flow_shows_delivered_and_autoreply");
    let thread = state.store.thread(Cid(1)).expect("лента Ани");
    assert_eq!(thread.unread, 0, "чат открыт — непрочитанных нет");
    let sent = thread
        .msgs
        .iter()
        .find(|m| m.body == "привет")
        .expect("моё сообщение в ленте");
    assert_eq!(
        sent.send,
        SendState::Delivered,
        "заглушка шлёт квази-ACK — как реальное ядро"
    );
    assert!(thread.msgs.iter().any(|m| m.body == "пинг"));
    assert!(thread.msgs.iter().any(|m| m.body == "принято"));
}

/// История (S6): демо-сцена несёт вчерашнюю переписку — при открытии
/// лента восстанавливается с разделителем дня, а статусы приходят из
/// флагов ядра (mine+delivered → «доставлено», mine без ACK →
/// «отправлено»). Окно исчерпано без хвостов.
fn history_restores_on_chat_open() {
    let program = app::program(Link::new(Scene::demo()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    // Аня: вчерашний диалог с доставкой.
    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Вчера".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("слушай, ты вчера был на созвоне?".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("был. договорились о сроках".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("доставлено".into())),
    );

    // Борис: своё сообщение без ACK — ядро отдало как «отправлено».
    let [press, release] = click("Борис");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("напиши, как доберёшься".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("отправлено".into())),
    );

    let state = finish(emu, &program, "history_restores_on_chat_open");
    let anna = state.store.thread(Cid(1)).expect("лента Ани");
    assert_eq!(
        state.store.history_loaded(Cid(1)),
        3,
        "вчерашняя история восстановлена"
    );
    assert_eq!(
        anna.msgs.len(),
        7,
        "3 из истории + 3 живых входящих + файл демо-сцены (S10)"
    );
    assert!(anna
        .msgs
        .iter()
        .any(|m| m.body == "слушай, ты вчера был на созвоне?"));
    assert!(anna
        .msgs
        .iter()
        .any(|m| m.body == "был. договорились о сроках"));
    assert!(
        !state.store.has_more_history(Cid(1)),
        "total == loaded — потолок ядра не достигнут"
    );
}

/// Инвайт-диалог: код заглушки отрисовался, чужой код гасится —
/// контакт появляется в сайдбаре и открывается.
fn invite_dialog_cycles_issue_and_add() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    // Кнопка сайдбара открывает диалог.
    let [press, release] = click("Пригласить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    // Код выдаётся по кнопке: сначала форма, потом ответ заглушки.
    let [press, release] = click("Выдать код");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Скопировать код".into())),
    );

    // Чужой код: клик по полю, ввод, «Добавить».
    let [press, release] = click("Вставьте код приглашения");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Typewrite(
            "shopot1.abc@192.168.1.5".into(),
        ))),
    );
    let [press, release] = click("Добавить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    pump(&mut emu, &program, &mut receiver, 1200);

    let state = finish(emu, &program, "invite_dialog_cycles_issue_and_add");
    let guest = state
        .store
        .contacts()
        .find(|c| c.name == "Гость 1")
        .expect("гость появился в сайдбаре");
    assert_eq!(
        state.store.selected(),
        Some(guest.cid),
        "новый контакт открыт"
    );
    assert!(state.overlay_closed(), "диалог закрыт после добавления");
}

/// Правая кнопка на строке контакта — меню: блокировка, карточка, закрытие.
fn right_click_menu_blocks_and_opens_card() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    let [press, release] = right_click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Заблокировать".into())),
    );

    let [press, release] = click("Заблокировать");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    pump(&mut emu, &program, &mut receiver, 500);

    // Меню снова: теперь «Разблокировать», открываем карточку.
    let [press, release] = right_click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Разблокировать".into())),
    );
    let [press, release] = click("Информация");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("заблокирован".into())),
    );

    // Разблок из карточки: карточка не закрывается, статус обновляется
    // в ней же (GUI_STYLE.md §2, честность статусов).
    let [press, release] = click("Разблокировать");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    pump(&mut emu, &program, &mut receiver, 500);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("в сети".into())),
    );

    let [press, release] = click("Закрыть");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    pump(&mut emu, &program, &mut receiver, 300);

    let state = finish(emu, &program, "right_click_menu_blocks_and_opens_card");
    assert!(
        !state.store.contact(Cid(1)).expect("Аня").blocked,
        "разблок через карточку дошёл до Store"
    );
}

/// Esc закрывает оверлей, пока композер в фокусе (GUI_UI.md §3).
/// Подписка клавиатуры в headless-эмуляторе событий не получает, поэтому
/// Esc проверяется через привязку композера; живой путь — подписка.
fn escape_closes_overlay() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );
    // Фокус композера: открыть чат.
    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    let [press, release] = click("Пригласить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    // Оверлей открыт: форма вместо мгновенного кода.
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Код появится после выдачи.".into())),
    );

    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Type(Key::Escape))),
    );
    pump(&mut emu, &program, &mut receiver, 300);

    let state = finish(emu, &program, "escape_closes_overlay");
    assert!(state.overlay_closed(), "Esc закрыл инвайт-диалог");
}

/// Настройки (S3): галки приватности и компакта, кнопки масштаба,
/// Esc закрывает панель. Значения сверяются по Store — рендер читает
/// его в том же кадре.
fn settings_overlay_toggles_and_closes() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );
    // Фокус композера: Esc пойдёт через его привязку (подписка в
    // headless-эмуляторе событий не получает).
    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    let [press, release] = click("Настройки");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Скрывать текст в уведомлениях".into())),
    );

    // Приватность: скрытие текста выключаем (по умолчанию включено).
    let [press, release] = click("Скрывать текст в уведомлениях");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    // Компактная лента: включаем.
    let [press, release] = click("Компактная лента — без пузырей, всё слева");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    // Масштаб: шаг вверх и шаг вниз.
    let [press, release] = click("Увеличить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    let [press, release] = click("Уменьшить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);

    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Type(Key::Escape))),
    );
    pump(&mut emu, &program, &mut receiver, 300);

    let state = finish(emu, &program, "settings_overlay_toggles_and_closes");
    assert!(state.overlay_closed(), "Esc закрыл панель настроек");
    let settings = &state.store.settings;
    assert!(
        !settings.hide_notification_text,
        "галка приватности переключена"
    );
    assert!(settings.compact, "компактная лента включена");
    assert!(
        (settings.font_scale - 1.0).abs() < 1e-6,
        "масштаб вернулся к 100%, а не {}",
        settings.font_scale
    );
}

/// Скриншот демо-сцены для ручной визуальной приёмки.
#[test]
fn screenshot_demo_scene() {
    let program = app::program(Link::new(Scene::demo()));
    let shot = iced_test::screenshot(
        &program,
        &iced::Theme::Dark,
        iced::Size::new(1000.0, 700.0),
        1.0,
        Duration::from_secs(2),
    );

    // Путь читает README (docs/screenshot.png): тест перегенерирует ассет.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/screenshot.png");
    save_png(shot, std::path::Path::new(path));
}

/// Звонок (S7): «Позвонить» → «звоним…» → авто-accept заглушки →
/// активный оверлей (таймер, mute, «Завершить»); Esc активный звонок не
/// закрывает (GUI_UI.md §3). Снимок активного звонка — визуальная приёмка.
fn call_overlay_and_mute() {
    let program = app::program(Link::new(Scene::smoke()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );

    // Открыть чат — в шапке появляется «Позвонить».
    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Позвонить".into())),
    );

    // Звоним: ответ через reply_delay сцены, «пир» примет ещё через столько
    // же — качаем с запасом.
    let [press, release] = click("Позвонить");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    pump(&mut emu, &program, &mut receiver, 800);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Завершить".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Выключить микрофон".into())),
    );

    // Esc активный звонок не закрывает.
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Interact(Interaction::Keyboard(Keyboard::Type(Key::Escape))),
    );
    pump(&mut emu, &program, &mut receiver, 300);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Завершить".into())),
    );

    // Mute-тумблер меняет подпись.
    let [press, release] = click("Выключить микрофон");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Включить микрофон".into())),
    );

    let state = finish(emu, &program, "call_active");
    assert!(state.call_active(), "звонок жив после Esc");
}

/// Разметка в ленте (S14): демо-сообщение с жирным/кодом/спойлером
/// приходит по сценарию и рендерится spans; снимок — визуальная приёмка.
fn formatted_message_renders() {
    let program = app::program(Link::new(Scene::demo()));
    let (sender, mut receiver) = mpsc::channel(512);
    let mut emu = Emulator::new(
        sender,
        &program,
        Mode::Immediate,
        iced::Size::new(1000.0, 700.0),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("Аня".into())),
    );
    let [press, release] = click("Аня");
    run_instruction(&mut emu, &program, &mut receiver, press);
    run_instruction(&mut emu, &program, &mut receiver, release);
    // Сценарные события демо приходят до 600 мс (разметка + реакция
    // «👍») — качаем с запасом.
    pump(&mut emu, &program, &mut receiver, 1200);
    // Реакция собеседника видна пилюлей под пузырём (S15), входящий
    // файл — пузырем с именем и размером (S10).
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("👍".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("📎 макет.pdf".into())),
    );
    run_instruction(
        &mut emu,
        &program,
        &mut receiver,
        Instruction::Expect(Expectation::Text("249 КБ".into())),
    );
    finish(emu, &program, "formatted_feed");
}

/// Только синтетический State: один и тот же текст на разных фонах,
/// без фоновых событий, сети и изменения пользовательских данных.
fn long_names_layout_matrix() {
    use shopot_gui::model::store::Contact;
    use shopot_gui::ui::{self, Message, SettingsIn};
    use std::sync::Arc;

    let boot = || {
        let mut state = ui::State::new(Arc::new(Link::new(Scene::smoke())));
        state.link_up = true;
        state.store.replace_contacts(vec![
            Contact {
                cid: Cid(1),
                name: "Александра / Alexandra — длинное имя без конца".into(),
                online: true,
                blocked: false,
                avatar_id: None,
            },
            Contact {
                cid: Cid(2),
                name: "William van der Longlastname Without Spaces".into(),
                online: false,
                blocked: false,
                avatar_id: None,
            },
        ]);
        state.store.select(Some(Cid(1)));
        (state, iced::Task::none())
    };
    let program =
        iced::application(boot, ui::update::<Link>, ui::view::<Link>).theme(iced::Theme::Dark);

    for (width, height) in [(1000, 700), (800, 560)] {
        for percent in [100, 120] {
            let suffix = format!("{width}x{height}_{percent}");
            let (sender, mut receiver) = mpsc::channel(512);
            let mut emu = Emulator::new(
                sender,
                &program,
                Mode::Immediate,
                iced::Size::new(width as f32, height as f32),
            );
            if percent == 120 {
                emu.update(
                    &program,
                    Message::Settings(SettingsIn::ChangeFontScale(0.2)),
                );
            }
            let expected = if percent == 100 {
                "Александра / Alexandra — дл…"
            } else {
                "Александра / Alexandra…"
            };
            run_instruction(
                &mut emu,
                &program,
                &mut receiver,
                Instruction::Expect(Expectation::Text(expected.into())),
            );
            let state = finish(emu, &program, &format!("long_names_{suffix}"));
            assert_eq!(
                state
                    .store
                    .contact(Cid(1))
                    .map(|contact| contact.name.as_str()),
                Some("Александра / Alexandra — длинное имя без конца"),
                "сокращается только отображение"
            );
        }
    }
}

fn neutral_text_matrix() {
    use shopot_gui::model::msg::{FileRef, MsgId};
    use shopot_gui::model::store::Contact;
    use shopot_gui::ui::{self, ChatIn, MeetIn, Message, SettingsIn};
    use std::sync::Arc;

    let boot = || {
        let mut state = ui::State::new(Arc::new(Link::new(Scene::smoke())));
        state.link_up = true;
        state.kernel_api = (2, 2);
        state.store.replace_contacts(vec![
            Contact {
                cid: Cid(1),
                name: "Александра / Alexandra — длинное имя".into(),
                online: false,
                blocked: false,
                avatar_id: None,
            },
            Contact {
                cid: Cid(2),
                name: "Борис / Boris".into(),
                online: false,
                blocked: false,
                avatar_id: None,
            },
        ]);
        state.store.select(Some(Cid(1)));
        state.store.push_incoming(
            Cid(1),
            "Александра / Alexandra — длинное имя",
            MsgId(100),
            1_000,
            "Жё/Шё: жёсткий срок.\nMixed RU/EN: проверка переноса длинной строки без потери текста.".into(),
        );
        let file = |name: &str, size, id| FileRef {
            name: name.into(),
            size,
            file_id: [id; 16],
            path: None,
        };
        state.store.push_incoming_file(
            Cid(1),
            "Александра / Alexandra — длинное имя",
            2_000,
            file("входящий.pdf", 2048, 1),
        );
        state
            .store
            .push_outgoing_file(Cid(1), 3_000, file("outgoing.pdf", 4096, 2));
        state.store.mark_file_sent(Cid(1), [2; 16]);
        (state, iced::Task::none())
    };
    let program =
        iced::application(boot, ui::update::<Link>, ui::view::<Link>).theme(iced::Theme::Dark);

    for (width, height) in [(1000, 700), (800, 560)] {
        for percent in [100, 120] {
            let suffix = format!("{width}x{height}_{percent}");
            for (page, message, expected) in [
                ("feed", None, "📎 входящий.pdf"),
                (
                    "compact",
                    Some(Message::Settings(SettingsIn::ToggleCompact(true))),
                    "📎 outgoing.pdf",
                ),
                (
                    "settings",
                    Some(Message::Settings(SettingsIn::Open)),
                    "Скрывать текст в уведомлениях",
                ),
                (
                    "invite",
                    Some(Message::Meet(MeetIn::OpenInvite)),
                    "Код появится после выдачи.",
                ),
                (
                    "search",
                    Some(Message::Chat(ChatIn::SearchOpen)),
                    "Поиск по всей истории чата.",
                ),
                (
                    "menu",
                    Some(Message::Meet(MeetIn::OpenMenu(Cid(1)))),
                    "Информация",
                ),
            ] {
                // iced_test 0.14 не восстанавливает cache после screenshot:
                // кадр всегда последний шаг отдельного эмулятора.
                let (sender, mut receiver) = mpsc::channel(512);
                let mut emu = Emulator::new(
                    sender,
                    &program,
                    Mode::Immediate,
                    iced::Size::new(width as f32, height as f32),
                );
                if percent == 120 {
                    emu.update(
                        &program,
                        Message::Settings(SettingsIn::ChangeFontScale(0.2)),
                    );
                }
                if let Some(message) = message {
                    emu.update(&program, message);
                }
                run_instruction(
                    &mut emu,
                    &program,
                    &mut receiver,
                    Instruction::Expect(Expectation::Text(expected.into())),
                );
                let state = finish(emu, &program, &format!("neutral_{page}_{suffix}"));
                assert!((state.store.settings.font_scale * 100.0 - percent as f32).abs() < 0.01);
            }
        }
    }
}
