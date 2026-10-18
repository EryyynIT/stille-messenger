//! ОС-уведомления о входящих: notify-send (libnotify), без новых
//! крейтов и без содержимого на диске (GUI_DEV.md §0). Аргументы
//! процесса живут в памяти, логи не пишутся.

use iced::Task;

use crate::model::cid::Cid;
use crate::ui::Message;

/// Уведомлять ли о входящем в чат `cid`: открытый в фокусе чат молчит,
/// без фокуса окна уведомляет любой чат (GUI_UI.md §4).
pub fn should_notify(selected: Option<Cid>, window_focused: bool, cid: Cid) -> bool {
    !window_focused || selected != Some(cid)
}

/// Тело уведомления: текст или обезличенная строка — по настройке
/// «скрывать текст» (приватность на общем экране, включена по умолчанию).
pub fn notification_body(text: &str, hide_text: bool) -> String {
    if hide_text {
        "Новое сообщение".to_owned()
    } else {
        text.to_owned()
    }
}

/// Отправить уведомление ОС. Сбой — не ошибка интерфейса: notify-send
/// может отсутствовать (headless-сервер), молча пропускаем. Крюк
/// SHOPOT_NO_NOTIFY отключает всплытия — нужен headless-тестам.
pub fn os_notify(title: String, body: String) -> Task<Message> {
    if std::env::var_os("SHOPOT_NO_NOTIFY").is_some() {
        return Task::none();
    }
    Task::perform(
        async move {
            let _ = tokio::process::Command::new("notify-send")
                .arg("--app-name=stille-messenger")
                .arg("--expire-time=5000")
                .arg(title)
                .arg(body)
                .output()
                .await;
        },
        |_| (),
    )
    .discard()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_chat_in_focused_window_is_silent() {
        assert!(!should_notify(Some(Cid(1)), true, Cid(1)));
        assert!(
            should_notify(Some(Cid(1)), true, Cid(2)),
            "другой чат — уведомляем"
        );
        assert!(
            should_notify(Some(Cid(1)), false, Cid(1)),
            "окно без фокуса — уведомляем даже открытый чат"
        );
        assert!(should_notify(None, true, Cid(2)));
    }

    #[test]
    fn hidden_text_becomes_neutral_line() {
        assert_eq!(notification_body("секрет", true), "Новое сообщение");
        assert_eq!(notification_body("секрет", false), "секрет");
    }
}
