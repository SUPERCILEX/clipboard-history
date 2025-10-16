#[macro_export]
macro_rules! icon {
    ($name:literal) => {{
        let bytes = include_bytes!(concat!("../../resources/icons/", $name, ".svg"));
        cosmic::widget::icon::from_svg_bytes(bytes).symbolic(true)
    }};
}

#[macro_export]
macro_rules! icon_app {
    ($name:literal) => {{
        let bytes = include_bytes!(concat!("../resources/icons/", $name, ".svg"));
        cosmic::widget::icon::from_svg_bytes(bytes).symbolic(true)
    }};
}
