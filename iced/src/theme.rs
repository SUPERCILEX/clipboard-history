use iced::{
    Background, Border, Color, Font, Padding, Shadow, Theme,
    widget::{button, container},
};
use native_theme_iced::{ResolvedTheme, from_preset, from_system};

/// Wraps a native-theme-derived iced theme and its resolved metrics.
#[derive(Clone, Debug)]
pub struct ThemeManager {
    pub theme: Theme,
    pub resolved: ResolvedTheme,
}

impl ThemeManager {
    pub fn new() -> Self {
        let (theme, resolved, _is_dark) = from_system().unwrap_or_else(|e| {
            eprintln!("Failed to load system theme: {e}, falling back to catppuccin-mocha");
            let (theme, resolved) = from_preset("catppuccin-mocha", true)
                .expect("catppuccin-mocha preset should be available");
            (theme, resolved, true)
        });
        Self { theme, resolved }
    }

    pub fn extended_palette(&self) -> &iced::theme::palette::Extended {
        self.theme.extended_palette()
    }

    pub fn border_radius(&self) -> f32 {
        native_theme_iced::border_radius(&self.resolved)
    }

    pub fn font(&self) -> Font {
        Font::default()
    }

    pub fn mono_font(&self) -> Font {
        Font::MONOSPACE
    }

    pub fn font_size(&self) -> f32 {
        native_theme_iced::font_size(&self.resolved)
    }

    pub fn mono_font_size(&self) -> f32 {
        native_theme_iced::mono_font_size(&self.resolved)
    }

    pub fn button_padding(&self) -> Padding {
        native_theme_iced::button_padding(&self.resolved)
    }

    pub fn input_padding(&self) -> Padding {
        native_theme_iced::input_padding(&self.resolved)
    }
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}

pub fn card_style(theme: &Theme, highlighted: bool, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    let bg = if highlighted {
        palette.primary.weak.color
    } else {
        palette.background.weak.color
    };
    container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: radius.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: false,
    }
}

/// Left-edge indicator strip for the keyboard-selected entry row.
pub fn accent_bar_style(theme: &Theme, active: bool, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(if active {
            palette.primary.strong.color
        } else {
            Color::TRANSPARENT
        })),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        snap: false,
        ..container::Style::default()
    }
}

pub fn section_header_style(theme: &Theme, _radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: None,
        border: Border::default(),
        text_color: Some(palette.background.base.text.scale_alpha(0.65)),
        snap: false,
        ..container::Style::default()
    }
}

/// A 1px hairline divider, e.g. under section headers.
pub fn divider_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        ..container::Style::default()
    }
}

pub fn error_banner_style(theme: &Theme, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.danger.weak.color)),
        border: Border {
            color: palette.danger.strong.color,
            width: 1.0,
            radius: radius.into(),
        },
        text_color: Some(palette.danger.strong.text),
        snap: false,
        ..container::Style::default()
    }
}

pub fn warning_banner_style(theme: &Theme, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.warning.weak.color)),
        border: Border {
            color: palette.warning.strong.color,
            width: 1.0,
            radius: radius.into(),
        },
        text_color: Some(palette.warning.strong.text),
        snap: false,
        ..container::Style::default()
    }
}

pub fn detail_panel_style(theme: &Theme, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        border: Border {
            color: palette.background.weak.text,
            width: 1.0,
            radius: radius.into(),
        },
        snap: false,
        ..container::Style::default()
    }
}

pub fn search_bar_style(theme: &Theme, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        border: Border {
            color: palette.background.weak.text,
            width: 1.0,
            radius: radius.into(),
        },
        snap: false,
        ..container::Style::default()
    }
}

pub fn badge_style(theme: &Theme, radius: f32, primary: bool) -> container::Style {
    let palette = theme.extended_palette();
    let (bg, fg) = if primary {
        (palette.primary.weak.color, palette.primary.strong.text)
    } else {
        (
            palette.background.strong.color,
            palette.background.base.text,
        )
    };
    container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            color: fg,
            width: 0.0,
            radius: radius.into(),
        },
        text_color: Some(fg),
        snap: false,
        ..container::Style::default()
    }
}

pub fn primary_button_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => palette.primary.strong.color,
        button::Status::Pressed => palette.primary.strong.color,
        _ => palette.primary.base.color,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: palette.primary.strong.text,
        border: Border {
            color: palette.primary.strong.color,
            width: 0.0,
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn secondary_button_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => palette.background.strong.color.scale_alpha(0.7),
        button::Status::Pressed => palette.background.strong.color,
        _ => palette.background.weak.color,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: palette.background.base.text,
        border: Border {
            color: palette.background.base.text,
            width: 0.0,
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn danger_button_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => palette.danger.weak.color,
        button::Status::Pressed => palette.danger.weak.color.scale_alpha(0.7),
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: palette.danger.base.color,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn icon_button_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => palette.background.weak.color,
        button::Status::Pressed => palette.background.strong.color,
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: palette.background.base.text,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: false,
    }
}

/// A pill-shaped segmented-control button, used for the tab bar and the
/// search-kind toggle. Quiet at rest; filled only when active.
pub fn pill_button_style(theme: &Theme, status: button::Status, is_active: bool) -> button::Style {
    let palette = theme.extended_palette();
    let (bg, text_color) = if is_active {
        (palette.primary.base.color, palette.primary.strong.text)
    } else {
        let bg = match status {
            button::Status::Hovered => palette.background.weak.color,
            button::Status::Pressed => palette.background.strong.color,
            _ => Color::TRANSPARENT,
        };
        (bg, palette.background.base.text.scale_alpha(0.85))
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: 999.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: false,
    }
}
