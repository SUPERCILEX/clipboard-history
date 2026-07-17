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

pub fn card_style(
    theme: &Theme,
    highlighted: bool,
    is_favorite: bool,
    radius: f32,
) -> container::Style {
    let palette = theme.extended_palette();
    let bg = if highlighted {
        palette.primary.weak.color
    } else {
        palette.background.weak.color
    };
    let border_color = if is_favorite {
        palette.primary.strong.color
    } else if highlighted {
        palette.primary.base.color
    } else {
        Color::TRANSPARENT
    };
    container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            color: border_color,
            width: if is_favorite || highlighted { 2.0 } else { 0.0 },
            radius: radius.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: false,
    }
}

pub fn section_header_style(theme: &Theme, radius: f32) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        snap: false,
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

pub fn primary_button_style(theme: &Theme, _status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: Some(Background::Color(palette.primary.base.color)),
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

pub fn secondary_button_style(theme: &Theme, _status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: Some(Background::Color(palette.background.strong.color)),
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

pub fn danger_button_style(theme: &Theme, _status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color: palette.danger.base.color,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn icon_button_style(theme: &Theme, _status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color: palette.background.base.text,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: false,
    }
}
