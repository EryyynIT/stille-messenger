//! Палитра и стили темы: iOS-подобный тёмный язык (GUI_STYLE.md §0).
//! Чёрный фон, синий акцент моих пузырей, капсульные радиусы, разделение
//! панелей заливкой без рамок. Цвета живут только здесь.

use iced::border::Radius;
use iced::widget::{button, checkbox, container, qr_code, text, text_editor, text_input};
use iced::{Background, Border, Color, Shadow, Theme};

/// Основной фон окна: почти чёрный, как тёмная тема iOS.
pub const BG: Color = Color::from_rgb8(0x0B, 0x0B, 0x0E);
/// Панели: сайдбар, шапки.
pub const PANEL: Color = Color::from_rgb8(0x16, 0x16, 0x1A);
/// Поднятые поверхности: выбранный чат, ховер, поля ввода.
pub const RAISE: Color = Color::from_rgb8(0x23, 0x23, 0x29);
/// Оверлей-панели (диалоги, карточки): на полтона светлее сайдбара.
pub const SHEET: Color = Color::from_rgb8(0x1C, 0x1C, 0x21);
/// Приглушённая линия: редко, где заливки не хватает.
pub const LINE: Color = Color::from_rgb8(0x2C, 0x2C, 0x33);
/// Основной текст.
pub const TEXT: Color = Color::from_rgb8(0xF2, 0xF2, 0xF7);
/// Второстепенный текст и плейсхолдеры на тёмных поверхностях (WCAG AA).
pub const MUTED: Color = Color::from_rgb8(0x98, 0x98, 0x9F);

/// Приглушённая графика; прежний цвет размера файла на синем пузыре.
pub const DIM: Color = Color::from_rgb8(0x63, 0x63, 0x6B);
/// Единственный акцент: синий iOS, он же пузырь моих сообщений.
pub const ACCENT: Color = Color::from_rgb8(0x0A, 0x84, 0xFF);
/// Семантика: онлайн / успех.
pub const OK: Color = Color::from_rgb8(0x30, 0xD1, 0x58);
/// Семантика: предупреждение, звонок (позже).
pub const WARN: Color = Color::from_rgb8(0xD9, 0xB4, 0x5C);
/// Семантика: ошибка, блокировка.
pub const ERR: Color = Color::from_rgb8(0xFF, 0x45, 0x3A);

/// Полужирный шрифт (только имена и активные элементы).
/// Эмодзи (S15/S10): моношрифтный fallback не красит COLRv1 — просим
/// монохромный «Noto Emoji» явно; без него в системе глиф пустой
/// (честный предел рендера).
pub fn emoji() -> iced::Font {
    iced::Font::with_name("Noto Emoji")
}

/// Пилюля реакции (S15): фон LINE (button_dim/RAISE сливается с
/// пузырём); своя — акцентный контур.
pub fn reaction_pill(mine: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_: &Theme, status: button::Status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if hovered { RAISE } else { LINE })),
            text_color: TEXT,
            border: Border {
                radius: Radius::from(10.0),
                width: if mine { 1.0 } else { 0.0 },
                color: if mine { ACCENT } else { LINE },
            },
            ..Default::default()
        }
    }
}

pub fn bold() -> iced::Font {
    iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::default()
    }
}

/// Фон всего окна.
pub fn root(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG)),
        ..Default::default()
    }
}

/// Панель (сайдбар, шапки): отделяется заливкой, рамок нет.
pub fn panel(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL)),
        ..Default::default()
    }
}

/// Пузырь чужого сообщения: тёмно-серый, крупный радиус.
pub fn bubble_theirs(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(0x26, 0x26, 0x2B))),
        border: Border {
            radius: Radius::from(18.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Пузырь моего сообщения: акцентный синий, как в iMessage.
pub fn bubble_mine(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(ACCENT)),
        border: Border {
            radius: Radius::from(18.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Бейдж непрочитанных: акцентная капсула с белой цифрой.
pub fn badge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(ACCENT)),
        text_color: Some(Color::WHITE),
        border: Border {
            radius: Radius::from(10.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Текст цвета `color`.
pub fn text_colored(color: Color) -> impl Fn(&Theme) -> text::Style {
    move |_| text::Style { color: Some(color) }
}

/// Кнопка-строка чата: прозрачная; ховер и выбор — капсула заливкой.
pub fn chat_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let background = if selected || hovered {
            Some(Background::Color(RAISE))
        } else {
            None
        };
        button::Style {
            background,
            text_color: TEXT,
            border: Border {
                radius: Radius::from(12.0),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// Длинный текст без пузыря: без фона и рамки.
pub fn no_bubble(_: &Theme) -> container::Style {
    container::Style {
        background: None,
        ..Default::default()
    }
}

/// Поле композера: капсула, фокус — тонкая акцентная обводка.
pub fn editor(_: &Theme, status: text_editor::Status) -> text_editor::Style {
    let focused = matches!(status, text_editor::Status::Focused { .. });
    text_editor::Style {
        background: Background::Color(RAISE),
        border: Border {
            color: if focused { ACCENT } else { LINE },
            width: 1.0,
            radius: Radius::from(20.0),
        },
        placeholder: MUTED,
        value: TEXT,
        selection: Color::from_rgba8(0x0A, 0x84, 0xFF, 0.35),
    }
}

/// Главная кнопка действия: акцентная капсула, белый текст.
pub fn button_accent(_: &Theme, status: button::Status) -> button::Style {
    let (bg, fg) = match status {
        button::Status::Disabled => (RAISE, MUTED),
        _ => (ACCENT, Color::WHITE),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: fg,
        border: Border {
            radius: Radius::from(18.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Кнопка второстепенного действия: серая капсула без рамки.
pub fn button_dim(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    button::Style {
        background: Some(Background::Color(if hovered { LINE } else { RAISE })),
        text_color: TEXT,
        border: Border {
            radius: Radius::from(18.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Прозрачная кнопка (имя контакта в шапке чата): ховер — поднятие.
pub fn button_plain(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    button::Style {
        background: hovered.then_some(Background::Color(RAISE)),
        text_color: TEXT,
        border: Border {
            radius: Radius::from(10.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Строка меню по правой кнопке: прозрачная, ховер — поднятие.
pub fn button_menu(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    button::Style {
        background: hovered.then_some(Background::Color(RAISE)),
        text_color: TEXT,
        border: Border {
            radius: Radius::from(10.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Тёмная подложка оверлея (GUI_UI.md §1).
pub fn backdrop(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba8(0x00, 0x00, 0x00, 0.55))),
        ..Default::default()
    }
}

/// Панель оверлея (диалог, карточка, меню): лист с круглыми углами
/// и мягкой тенью, как шторка в iOS.
pub fn dialog(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SHEET)),
        border: Border {
            radius: Radius::from(16.0),
            ..Default::default()
        },
        shadow: Shadow {
            color: Color::from_rgba8(0x00, 0x00, 0x00, 0.45),
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..Default::default()
    }
}

/// Однострочное поле ввода (код приглашения в диалоге).
pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(RAISE),
        border: Border {
            color: if focused { ACCENT } else { LINE },
            width: 1.0,
            radius: Radius::from(12.0),
        },
        icon: MUTED,
        placeholder: MUTED,
        value: TEXT,
        selection: Color::from_rgba8(0x0A, 0x84, 0xFF, 0.35),
    }
}

/// Чекбокс диалога: круглые углы, отметка акцентом.
pub fn checkbox_style(_: &Theme, status: checkbox::Status) -> checkbox::Style {
    let text_color = match status {
        checkbox::Status::Disabled { .. } => MUTED,
        _ => TEXT,
    };
    checkbox::Style {
        background: Background::Color(RAISE),
        icon_color: ACCENT,
        border: Border {
            color: LINE,
            width: 1.0,
            radius: Radius::from(6.0),
        },
        text_color: Some(text_color),
    }
}

/// QR-код: тёмные модули на белом — читается камерой с экрана.
pub fn qr(_: &Theme) -> qr_code::Style {
    qr_code::Style {
        cell: Color::BLACK,
        background: Color::WHITE,
    }
}

/// Невидимая кнопка-подложка оверлея: только ловит клик мимо панели.
pub fn button_invisible(_: &Theme, _: button::Status) -> button::Style {
    button::Style {
        background: None,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f64 {
        let linear = |channel: f32| {
            let channel = f64::from(channel);
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn assert_aa(foreground: Color, background: Color) {
        let (fg, bg) = (luminance(foreground), luminance(background));
        let contrast = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
        assert!(contrast >= 4.5, "контраст {contrast:.2}:1 ниже WCAG AA");
    }

    #[test]
    fn secondary_text_meets_aa_on_dark_surfaces() {
        let theirs = match bubble_theirs(&Theme::Dark).background.unwrap() {
            Background::Color(color) => color,
            _ => panic!("пузырь должен иметь сплошную заливку"),
        };
        for surface in [BG, PANEL, RAISE, SHEET, LINE, theirs] {
            assert_aa(MUTED, surface);
        }
    }

    #[test]
    fn input_placeholders_meet_aa() {
        let input = input(&Theme::Dark, text_input::Status::Active);
        let editor = editor(&Theme::Dark, text_editor::Status::Active);
        for (placeholder, background) in [
            (input.placeholder, input.background),
            (editor.placeholder, editor.background),
        ] {
            let Background::Color(background) = background else {
                panic!("поле должно иметь сплошную заливку");
            };
            assert_eq!(placeholder, MUTED);
            assert_aa(placeholder, background);
        }
    }
}
