pub mod server {
    use std::{
        fs::File,
        io,
        io::{ErrorKind, Read},
        num::NonZeroU32,
        path::Path,
    };

    use ringboard_core::{IoErr, protocol::RingKind};
    use stable_type::stable_type;

    #[must_use]
    pub const fn file_name() -> &'static str {
        "config.toml"
    }

    pub fn load<P: AsRef<Path>>(path: P) -> crate::core::Result<Config> {
        let path = path.as_ref();
        let mut file = match File::open(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            r => r.map_io_err(|| format!("Failed to open file: {path:?}"))?,
        };

        let mut config = String::new();
        file.read_to_string(&mut config)
            .map_io_err(|| format!("Failed to read config: {path:?}"))?;
        Ok(toml::from_str::<Stable>(&config)
            .map_err(|error| crate::core::Error::Io {
                error: io::Error::new(ErrorKind::InvalidData, error),
                context: format!("Failed to parse server config: {path:?}").into(),
            })?
            .into())
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    pub struct MaxEntries {
        pub main: NonZeroU32,
        pub favorites: NonZeroU32,
    }

    stable_type! {
        #[derive(Debug)]
        pub struct Config [
            "1": { pub max_entries: MaxEntries },
        ]
    }

    impl Config {
        #[must_use]
        pub const fn max_entries(&self, kind: RingKind) -> NonZeroU32 {
            let MaxEntries { main, favorites } = self.max_entries;
            match kind {
                RingKind::Main => main,
                RingKind::Favorites => favorites,
            }
        }
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                max_entries: MaxEntries {
                    main: NonZeroU32::new(131_070).unwrap(),
                    favorites: NonZeroU32::new(1022).unwrap(),
                },
            }
        }
    }
}

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
