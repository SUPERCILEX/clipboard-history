pub mod x11 {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use serde::{Deserialize, Serialize};

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("x11.toml");
        file
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct V1 {
        auto_paste: bool,
        fast_path_optimizations: bool,
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "version")]
    enum Versions {
        #[serde(rename = "1", alias = "V1")]
        V1(V1),
    }

    type Latest = V1;

    #[derive(Debug)]
    pub struct Config {
        pub auto_paste: bool,
        pub fast_path_optimizations: bool,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                auto_paste: true,
                fast_path_optimizations: true,
            }
        }
    }

    impl From<Latest> for Config {
        fn from(
            Latest {
                auto_paste,
                fast_path_optimizations,
            }: Latest,
        ) -> Self {
            Self {
                auto_paste,
                fast_path_optimizations,
            }
        }
    }

    impl From<Config> for Latest {
        fn from(
            Config {
                auto_paste,
                fast_path_optimizations,
            }: Config,
        ) -> Self {
            Self {
                auto_paste,
                fast_path_optimizations,
            }
        }
    }

    impl Default for Versions {
        fn default() -> Self {
            Self::V1(Config::default().into())
        }
    }

    #[derive(Default, Serialize, Deserialize, Debug)]
    #[serde(transparent)]
    pub struct Stable(Versions);

    impl From<Stable> for Config {
        fn from(value: Stable) -> Self {
            match value.0 {
                Versions::V1(c) => c.into(),
            }
        }
    }

    impl From<Config> for Stable {
        fn from(value: Config) -> Self {
            Self(Versions::V1(value.into()))
        }
    }
}

pub mod wayland {
    use std::path::PathBuf;

    use ringboard_core::dirs::config_file_dir;
    use serde::{Deserialize, Serialize};

    #[must_use]
    pub fn file() -> PathBuf {
        let mut file = config_file_dir();
        file.push("wayland.toml");
        file
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct V1 {
        auto_paste: bool,
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "version")]
    enum Versions {
        #[serde(rename = "1")]
        V1(V1),
    }

    type Latest = V1;

    #[derive(Debug)]
    pub struct Config {
        pub auto_paste: bool,
    }

    impl Default for Config {
        fn default() -> Self {
            Self { auto_paste: true }
        }
    }

    impl From<Latest> for Config {
        fn from(Latest { auto_paste }: Latest) -> Self {
            Self { auto_paste }
        }
    }

    impl From<Config> for Latest {
        fn from(Config { auto_paste }: Config) -> Self {
            Self { auto_paste }
        }
    }

    impl Default for Versions {
        fn default() -> Self {
            Self::V1(Config::default().into())
        }
    }

    #[derive(Default, Serialize, Deserialize, Debug)]
    #[serde(transparent)]
    pub struct Stable(Versions);

    impl From<Stable> for Config {
        fn from(value: Stable) -> Self {
            match value.0 {
                Versions::V1(c) => c.into(),
            }
        }
    }

    impl From<Config> for Stable {
        fn from(value: Config) -> Self {
            Self(Versions::V1(value.into()))
        }
    }
}
