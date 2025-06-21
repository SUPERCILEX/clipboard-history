pub mod x11 {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use serde::{Deserialize, Serialize};

    use crate::config::{auto_paste_, fast_path_optimizations_};

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("x11.toml");
        file
    }

    #[derive(Serialize, Deserialize, Debug)]
    pub struct V1 {
        #[serde(default = "auto_paste_")]
        pub auto_paste: bool,
        #[serde(default = "fast_path_optimizations_")]
        pub fast_path_optimizations: bool,
    }

    impl Default for V1 {
        fn default() -> Self {
            Self {
                auto_paste: auto_paste_(),
                fast_path_optimizations: fast_path_optimizations_(),
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "version")]
    pub enum Config {
        V1(V1),
    }

    impl Config {
        #[must_use]
        pub const fn to_latest(self) -> Latest {
            match self {
                Self::V1(c) => c,
            }
        }
    }

    impl Default for Config {
        fn default() -> Self {
            Self::V1(V1::default())
        }
    }

    pub type Latest = V1;
}

pub mod wayland {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use serde::{Deserialize, Serialize};

    use crate::config::auto_paste_;

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("wayland.toml");
        file
    }

    #[derive(Serialize, Deserialize, Debug)]
    pub struct V1 {
        #[serde(default = "auto_paste_")]
        pub auto_paste: bool,
    }

    impl Default for V1 {
        fn default() -> Self {
            Self {
                auto_paste: auto_paste_(),
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "version")]
    pub enum Config {
        #[serde(rename = "1")]
        V1(V1),
    }

    impl Config {
        #[must_use]
        pub const fn to_latest(self) -> Latest {
            match self {
                Self::V1(c) => c,
            }
        }
    }

    impl Default for Config {
        fn default() -> Self {
            Self::V1(V1::default())
        }
    }

    pub type Latest = V1;
}

const fn auto_paste_() -> bool {
    true
}

const fn fast_path_optimizations_() -> bool {
    true
}
