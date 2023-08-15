// SPDX-License-Identifier: GPL-3.0-or-later

mod api_handlers;

use std::{
    cell::RefCell,
    error::Error,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    api::{
        msg::{CallbackId, Msg},
        PinnacleSocketSource,
    },
    cursor::Cursor,
    focus::FocusState,
    grab::resize_grab::ResizeSurfaceState,
    window::{window_state::LocationRequestState, WindowElement},
};
use calloop::futures::Scheduler;
use smithay::{
    backend::renderer::element::RenderElementStates,
    desktop::{
        utils::{
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
            OutputPresentationFeedback,
        },
        PopupManager, Space,
    },
    input::{keyboard::XkbConfig, pointer::CursorImageStatus, Seat, SeatState},
    output::Output,
    reexports::{
        calloop::{
            self, channel::Event, generic::Generic, Interest, LoopHandle, LoopSignal, Mode,
            PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Clock, IsAlive, Logical, Monotonic, Point, Size},
    wayland::{
        compositor::{self, CompositorClientState, CompositorState},
        data_device::DataDeviceState,
        dmabuf::DmabufFeedback,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        primary_selection::PrimarySelectionState,
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};

use crate::{backend::Backend, input::InputState};

/// The main state of the application.
pub struct State<B: Backend> {
    pub backend_data: B,

    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, CalloopData<B>>,
    pub display_handle: DisplayHandle,
    pub clock: Clock<Monotonic>,

    pub space: Space<WindowElement>,
    pub move_mode: bool,
    pub socket_name: String,

    pub seat: Seat<State<B>>,

    pub compositor_state: CompositorState,
    pub data_device_state: DataDeviceState,
    pub seat_state: SeatState<Self>,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub xdg_shell_state: XdgShellState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub primary_selection_state: PrimarySelectionState,
    pub layer_shell_state: WlrLayerShellState,

    pub input_state: InputState,
    pub api_state: ApiState,
    pub focus_state: FocusState,

    pub popup_manager: PopupManager,

    pub cursor_status: CursorImageStatus,
    pub pointer_location: Point<f64, Logical>,
    pub dnd_icon: Option<WlSurface>,

    pub windows: Vec<WindowElement>,

    pub async_scheduler: Scheduler<()>,

    // TODO: move into own struct
    // |     basically just clean this mess up
    pub output_callback_ids: Vec<CallbackId>,

    pub xwayland: XWayland,
    pub xwm: Option<X11Wm>,
    pub xdisplay: Option<u32>,
}

/// Schedule something to be done when windows have finished committing and have become
/// idle.
pub fn schedule_on_commit<F, B: Backend>(
    data: &mut CalloopData<B>,
    windows: Vec<WindowElement>,
    on_commit: F,
) where
    F: FnOnce(&mut CalloopData<B>) + 'static,
{
    for window in windows.iter().filter(|win| win.alive()) {
        if window.with_state(|state| !matches!(state.loc_request_state, LocationRequestState::Idle))
        {
            // tracing::debug!(
            //     "window state is {:?}",
            //     window.with_state(|state| state.loc_request_state.clone())
            // );
            data.state.loop_handle.insert_idle(|data| {
                schedule_on_commit(data, windows, on_commit);
            });
            return;
        }
    }

    on_commit(data);
}

// Schedule something to be done when `condition` returns true.
pub fn schedule<F1, F2, B: Backend>(data: &mut CalloopData<B>, condition: F1, run: F2)
where
    F1: Fn(&mut CalloopData<B>) -> bool + 'static,
    F2: FnOnce(&mut CalloopData<B>) + 'static,
{
    if !condition(data) {
        data.state.loop_handle.insert_idle(|data| {
            schedule(data, condition, run);
        });
        return;
    }

    run(data);
}

impl<B: Backend> State<B> {
    pub fn init(
        backend_data: B,
        display: &mut Display<Self>,
        loop_signal: LoopSignal,
        loop_handle: LoopHandle<'static, CalloopData<B>>,
    ) -> Result<Self, Box<dyn Error>> {
        let socket = ListeningSocketSource::new_auto()?;
        let socket_name = socket.socket_name().to_os_string();

        std::env::set_var("WAYLAND_DISPLAY", socket_name.clone());
        tracing::info!(
            "Set WAYLAND_DISPLAY to {}",
            socket_name.clone().to_string_lossy()
        );

        // Opening a new process will use up a few file descriptors, around 10 for Alacritty, for
        // example. Because of this, opening up only around 100 processes would exhaust the file
        // descriptor limit on my system (Arch btw) and cause a "Too many open files" crash.
        //
        // To fix this, I just set the limit to be higher. As Pinnacle is the whole graphical
        // environment, I *think* this is ok.
        tracing::info!("Trying to raise file descriptor limit...");
        if let Err(err) = smithay::reexports::nix::sys::resource::setrlimit(
            smithay::reexports::nix::sys::resource::Resource::RLIMIT_NOFILE,
            65536,
            65536 * 2,
        ) {
            tracing::error!("Could not raise fd limit: errno {err}");
        } else {
            tracing::info!("Fd raise success!");
        }

        loop_handle.insert_source(socket, |stream, _metadata, data| {
            data.display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
                .expect("Could not insert client into loop handle");
        })?;

        loop_handle.insert_source(
            Generic::new(
                display.backend().poll_fd().as_raw_fd(),
                Interest::READ,
                Mode::Level,
            ),
            |_readiness, _metadata, data| {
                data.display.dispatch_clients(&mut data.state)?;
                Ok(PostAction::Continue)
            },
        )?;

        let (tx_channel, rx_channel) = calloop::channel::channel::<Msg>();

        // We want to replace the client if a new one pops up
        // TODO: there should only ever be one client working at a time, and creating a new client
        // |     when one is already running should be impossible.
        // INFO: this source try_clone()s the stream

        // TODO: probably use anyhow or something
        let socket_source = match PinnacleSocketSource::new(tx_channel) {
            Ok(source) => source,
            Err(err) => {
                tracing::error!("Failed to create the socket source: {err}");
                Err(err)?
            }
        };

        loop_handle.insert_source(socket_source, |stream, _, data| {
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

        let (executor, sched) =
            calloop::futures::executor::<()>().expect("Couldn't create executor");
        loop_handle.insert_source(executor, |_, _, _| {})?;

        start_config()?;
        // start_lua_config()?;

        let display_handle = display.handle();
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, backend_data.seat_name());
        seat.add_pointer();
        seat.add_keyboard(XkbConfig::default(), 200, 25)?;

        loop_handle.insert_idle(|data| {
            data.state
                .loop_handle
                .insert_source(rx_channel, |msg, _, data| match msg {
                    Event::Msg(msg) => data.state.handle_msg(msg),
                    Event::Closed => todo!(),
                })
                .expect("failed to insert rx_channel into loop");
        });

        tracing::debug!("before xwayland");
        let xwayland = {
            let (xwayland, channel) = XWayland::new(&display_handle);
            let clone = display_handle.clone();
            tracing::debug!("inserting into loop");
            let res = loop_handle.insert_source(channel, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    connection,
                    client,
                    client_fd: _,
                    display,
                } => {
                    tracing::debug!("XWaylandEvent ready");
                    let mut wm = X11Wm::start_wm(
                        data.state.loop_handle.clone(),
                        clone.clone(),
                        connection,
                        client,
                    )
                    .expect("failed to attach x11wm");
                    let cursor = Cursor::load();
                    let image = cursor.get_image(1, Duration::ZERO);
                    wm.set_cursor(
                        &image.pixels_rgba,
                        Size::from((image.width as u16, image.height as u16)),
                        Point::from((image.xhot as u16, image.yhot as u16)),
                    )
                    .expect("failed to set xwayland default cursor");
                    tracing::debug!("setting xwm and xdisplay");
                    data.state.xwm = Some(wm);
                    data.state.xdisplay = Some(display);
                }
                XWaylandEvent::Exited => {
                    data.state.xwm.take();
                }
            });
            if let Err(err) = res {
                tracing::error!("Failed to insert XWayland source into loop: {err}");
            }
            xwayland
        };
        tracing::debug!("after xwayland");

        Ok(Self {
            backend_data,
            loop_signal,
            loop_handle,
            display_handle: display_handle.clone(),
            clock: Clock::<Monotonic>::new()?,
            compositor_state: CompositorState::new::<Self>(&display_handle),
            data_device_state: DataDeviceState::new::<Self>(&display_handle),
            seat_state,
            pointer_location: (0.0, 0.0).into(),
            shm_state: ShmState::new::<Self>(&display_handle, vec![]),
            space: Space::<WindowElement>::default(),
            cursor_status: CursorImageStatus::Default,
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&display_handle),
            xdg_shell_state: XdgShellState::new::<Self>(&display_handle),
            viewporter_state: ViewporterState::new::<Self>(&display_handle),
            fractional_scale_manager_state: FractionalScaleManagerState::new::<Self>(
                &display_handle,
            ),
            primary_selection_state: PrimarySelectionState::new::<Self>(&display_handle),
            layer_shell_state: WlrLayerShellState::new::<Self>(&display_handle),

            input_state: InputState::new(),
            api_state: ApiState::new(),
            focus_state: FocusState::new(),

            seat,

            dnd_icon: None,

            move_mode: false,
            socket_name: socket_name.to_string_lossy().to_string(),

            popup_manager: PopupManager::default(),

            async_scheduler: sched,

            windows: vec![],
            output_callback_ids: vec![],

            xwayland,
            xwm: None,
            xdisplay: None,
        })
    }
}

fn start_config() -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = {
        let config_dir = std::env::var("PINNACLE_CONFIG_DIR").unwrap_or_else(|_| {
            let default_config_dir =
                std::env::var("XDG_CONFIG_HOME").unwrap_or("~/.config".to_string());

            PathBuf::from(default_config_dir)
                .join("pinnacle")
                .to_string_lossy()
                .to_string()
        });
        PathBuf::from(shellexpand::tilde(&config_dir).to_string())
    };

    let metaconfig = crate::metaconfig::parse(&config_dir)?;

    let handle = std::thread::spawn(move || {
        let mut command = metaconfig.command.split(' ');

        let arg1 = command.next().expect("empty command");

        std::env::set_current_dir(&config_dir).expect("failed to cd");

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
                            |var| Ok::<_, ()>(Some(std::env::var(var).unwrap_or("".to_string()))),
                        )
                        .ok()?
                        .to_string(),
                    ))
                } else {
                    None
                }
            });

        let mut child = std::process::Command::new(arg1)
            .args(command)
            .envs(envs)
            .spawn()
            .expect("failed to spawn");
        let _ = child.wait();
    });

    Ok(())
}

pub struct CalloopData<B: Backend> {
    pub display: Display<State<B>>,
    pub state: State<B>,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

#[derive(Debug, Copy, Clone)]
pub struct SurfaceDmabufFeedback<'a> {
    pub render_feedback: &'a DmabufFeedback,
    pub scanout_feedback: &'a DmabufFeedback,
}

// TODO: docs
pub fn take_presentation_feedback(
    output: &Output,
    space: &Space<WindowElement>,
    render_element_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_presentation_feedback = OutputPresentationFeedback::new(output);

    space.elements().for_each(|window| {
        if space.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }
    });

    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.take_presentation_feedback(
            &mut output_presentation_feedback,
            surface_primary_scanout_output,
            |surface, _| {
                surface_presentation_feedback_flags_from_states(surface, render_element_states)
            },
        );
    }

    output_presentation_feedback
}

/// State containing the config API's stream.
#[derive(Default)]
pub struct ApiState {
    // TODO: this may not need to be in an arc mutex because of the move to async
    pub stream: Option<Arc<Mutex<UnixStream>>>,
}

impl ApiState {
    pub fn new() -> Self {
        Default::default()
    }
}

pub trait WithState {
    type State: Default;
    fn with_state<F, T>(&self, func: F) -> T
    where
        F: FnMut(&mut Self::State) -> T;
}

#[derive(Default, Debug)]
pub struct WlSurfaceState {
    pub resize_state: ResizeSurfaceState,
}

impl WithState for WlSurface {
    type State = WlSurfaceState;

    fn with_state<F, T>(&self, mut func: F) -> T
    where
        F: FnMut(&mut Self::State) -> T,
    {
        compositor::with_states(self, |states| {
            states
                .data_map
                .insert_if_missing(RefCell::<Self::State>::default);
            let state = states
                .data_map
                .get::<RefCell<Self::State>>()
                .expect("This should never happen");

            func(&mut state.borrow_mut())
        })
    }
}
