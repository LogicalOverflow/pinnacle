pub mod api;

use crate::config::api::{msg::ModifierMask, PinnacleSocketSource};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Context;
use calloop::channel::Sender;
use smithay::input::keyboard::keysyms;
use toml::Table;

use api::msg::Modifier;

use crate::{
    state::{State, WithState},
    tag::TagId,
};

#[derive(serde::Deserialize, Debug)]
pub struct Metaconfig {
    pub command: String,
    pub envs: Option<Table>,
    pub reload_keybind: Keybind,
    pub kill_keybind: Keybind,
    pub socket_dir: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
pub struct Keybind {
    pub modifiers: Vec<Modifier>,
    pub key: Key,
}

#[derive(serde::Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Key {
    A = keysyms::KEY_a,
    B = keysyms::KEY_b,
    C = keysyms::KEY_c,
    D = keysyms::KEY_d,
    E = keysyms::KEY_e,
    F = keysyms::KEY_f,
    G = keysyms::KEY_g,
    H = keysyms::KEY_h,
    I = keysyms::KEY_i,
    J = keysyms::KEY_j,
    K = keysyms::KEY_k,
    L = keysyms::KEY_l,
    M = keysyms::KEY_m,
    N = keysyms::KEY_n,
    O = keysyms::KEY_o,
    P = keysyms::KEY_p,
    Q = keysyms::KEY_q,
    R = keysyms::KEY_r,
    S = keysyms::KEY_s,
    T = keysyms::KEY_t,
    U = keysyms::KEY_u,
    V = keysyms::KEY_v,
    W = keysyms::KEY_w,
    X = keysyms::KEY_x,
    Y = keysyms::KEY_y,
    Z = keysyms::KEY_z,
    #[serde(alias = "0")]
    Zero = keysyms::KEY_0,
    #[serde(alias = "1")]
    One = keysyms::KEY_1,
    #[serde(alias = "2")]
    Two = keysyms::KEY_2,
    #[serde(alias = "3")]
    Three = keysyms::KEY_3,
    #[serde(alias = "4")]
    Four = keysyms::KEY_4,
    #[serde(alias = "5")]
    Five = keysyms::KEY_5,
    #[serde(alias = "6")]
    Six = keysyms::KEY_6,
    #[serde(alias = "7")]
    Seven = keysyms::KEY_7,
    #[serde(alias = "8")]
    Eight = keysyms::KEY_8,
    #[serde(alias = "9")]
    Nine = keysyms::KEY_9,
    #[serde(alias = "num0")]
    NumZero = keysyms::KEY_KP_0,
    #[serde(alias = "num1")]
    NumOne = keysyms::KEY_KP_1,
    #[serde(alias = "num2")]
    NumTwo = keysyms::KEY_KP_2,
    #[serde(alias = "num3")]
    NumThree = keysyms::KEY_KP_3,
    #[serde(alias = "num4")]
    NumFour = keysyms::KEY_KP_4,
    #[serde(alias = "num5")]
    NumFive = keysyms::KEY_KP_5,
    #[serde(alias = "num6")]
    NumSix = keysyms::KEY_KP_6,
    #[serde(alias = "num7")]
    NumSeven = keysyms::KEY_KP_7,
    #[serde(alias = "num8")]
    NumEight = keysyms::KEY_KP_8,
    #[serde(alias = "num9")]
    NumNine = keysyms::KEY_KP_9,
    #[serde(alias = "esc")]
    Escape = keysyms::KEY_Escape,
}

fn parse(config_dir: &Path) -> anyhow::Result<Metaconfig> {
    let config_dir = config_dir.join("metaconfig.toml");

    let metaconfig =
        std::fs::read_to_string(config_dir).context("Failed to read metaconfig.toml")?;

    toml::from_str(&metaconfig).context("Failed to deserialize toml")
}

fn get_config_dir() -> PathBuf {
    let config_dir = std::env::var("PINNACLE_CONFIG_DIR")
        .ok()
        .and_then(|s| Some(PathBuf::from(shellexpand::full(&s).ok()?.to_string())));

    config_dir.unwrap_or(crate::XDG_BASE_DIRS.get_config_home())
}

pub fn start_config(tx_channel: Sender<api::msg::Msg>) -> anyhow::Result<ConfigReturn> {
    let config_dir = get_config_dir();
    tracing::debug!("config dir is {:?}", config_dir);

    let metaconfig = parse(&config_dir)?;

    // If a socket is provided in the metaconfig, use it.
    let socket_dir = if let Some(socket_dir) = &metaconfig.socket_dir {
        // cd into the metaconfig dir and canonicalize to preserve relative paths
        // like ./dir/here
        let current_dir = std::env::current_dir()?;

        std::env::set_current_dir(&config_dir)?;
        let socket_dir = PathBuf::from(socket_dir).canonicalize()?;
        std::env::set_current_dir(current_dir)?;
        socket_dir
    } else {
        // Otherwise, use $XDG_RUNTIME_DIR. If that doesn't exist, use /tmp.
        crate::XDG_BASE_DIRS
            .get_runtime_directory()
            .cloned()
            .unwrap_or(PathBuf::from(crate::config::api::DEFAULT_SOCKET_DIR))
    };

    let socket_source = PinnacleSocketSource::new(tx_channel, &socket_dir)
        .context("Failed to create socket source")?;

    let reload_keybind = metaconfig.reload_keybind;
    let kill_keybind = metaconfig.kill_keybind;

    let mut command = metaconfig.command.split(' ');

    let arg1 = command
        .next()
        .context("command in metaconfig.toml was empty")?;

    std::env::set_var("PINNACLE_DIR", std::env::current_dir()?);

    let envs = metaconfig
        .envs
        .unwrap_or(toml::map::Map::new())
        .into_iter()
        .filter_map(|(key, val)| {
            if let toml::Value::String(string) = val {
                Some((
                    key,
                    shellexpand::full_with_context(
                        &string,
                        || std::env::var("HOME").ok(),
                        // Expand nonexistent vars to an empty string instead of crashing
                        |var| Ok::<_, ()>(Some(std::env::var(var).unwrap_or("".to_string()))),
                    )
                    .ok()?
                    .to_string(),
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    tracing::debug!("Config envs are {:?}", envs);

    // Using async_process's Child instead of std::process because I don't have to spawn my own
    // thread to wait for the child
    let child = async_process::Command::new(arg1)
        .args(command)
        .envs(envs)
        .current_dir(config_dir)
        .stdout(async_process::Stdio::inherit())
        .stderr(async_process::Stdio::inherit())
        .spawn()
        .expect("failed to spawn config");

    tracing::info!("Started config with {}", metaconfig.command);

    let reload_mask = ModifierMask::from(reload_keybind.modifiers);
    let kill_mask = ModifierMask::from(kill_keybind.modifiers);

    Ok(ConfigReturn {
        reload_keybind: (reload_mask, reload_keybind.key as u32),
        kill_keybind: (kill_mask, kill_keybind.key as u32),
        config_child_handle: child,
        socket_source,
    })
}

pub struct ConfigReturn {
    pub reload_keybind: (ModifierMask, u32),
    pub kill_keybind: (ModifierMask, u32),
    pub config_child_handle: async_process::Child,
    pub socket_source: PinnacleSocketSource,
}

impl State {
    pub fn restart_config(&mut self) -> anyhow::Result<()> {
        tracing::info!("Restarting config");
        tracing::debug!("Clearing tags");

        for output in self.space.outputs() {
            output.with_state(|state| state.tags.clear());
        }

        TagId::reset();

        tracing::debug!("Clearing mouse and keybinds");
        self.input_state.keybinds.clear();
        self.input_state.mousebinds.clear();
        self.window_rules.clear();

        tracing::debug!("Killing old config");
        if let Err(err) = self.api_state.config_process.kill() {
            tracing::warn!("Error when killing old config: {err}");
        }

        self.loop_handle.remove(self.api_state.socket_token);

        let ConfigReturn {
            reload_keybind,
            kill_keybind,
            config_child_handle,
            socket_source,
        } = start_config(self.api_state.tx_channel.clone())?;

        let socket_token = self
            .loop_handle
            .insert_source(socket_source, |stream, _, data| {
                if let Some(old_stream) = data
                    .state
                    .api_state
                    .stream
                    .replace(Arc::new(Mutex::new(stream)))
                {
                    old_stream
                        .lock()
                        .expect("Couldn't lock old stream")
                        .shutdown(std::net::Shutdown::Both)
                        .expect("Couldn't shutdown old stream");
                }
            })?;

        self.input_state.reload_keybind = reload_keybind;
        self.input_state.kill_keybind = kill_keybind;
        self.api_state.config_process = config_child_handle;
        self.api_state.socket_token = socket_token;

        Ok(())
    }
}
