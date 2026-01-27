pub mod x11 {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use stable_type::stable_type;

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("x11.toml");
        file
    }

    stable_type! {
        #[derive(Debug)]
        pub struct Config [
            "1": { pub auto_paste: bool, pub fast_path_optimizations: bool },
        ]
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                auto_paste: true,
                fast_path_optimizations: true,
            }
        }
    }
}

pub mod wayland {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use stable_type::stable_type;

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("wayland.toml");
        file
    }

    stable_type! {
        #[derive(Debug)]
        pub struct Config [
            "1": { pub auto_paste: bool },
        ]
    }

    impl Default for Config {
        fn default() -> Self {
            Self { auto_paste: true }
        }
    }
}
