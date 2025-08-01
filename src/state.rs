// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    backend::{
        kms::{KmsGuard, KmsState},
        render::{GlMultiError, RendererRef},
        winit::WinitState,
        x11::X11State,
    },
    config::{Config, OutputConfig, OutputState, ScreenFilter},
    input::{gestures::GestureState, PointerFocusState},
    shell::{grabs::SeatMoveGrabState, CosmicSurface, SeatExt, Shell},
    utils::prelude::OutputExt,
    wayland::{
        handlers::{data_device::get_dnd_icon, screencopy::SessionHolder},
        protocols::{
            a11y::A11yState,
            atspi::AtspiState,
            drm::WlDrmState,
            image_capture_source::ImageCaptureSourceState,
            output_configuration::OutputConfigurationState,
            output_power::OutputPowerState,
            overlap_notify::OverlapNotifyState,
            screencopy::ScreencopyState,
            toplevel_info::ToplevelInfoState,
            toplevel_management::{ManagementCapabilities, ToplevelManagementState},
            workspace::{WorkspaceState, WorkspaceUpdateGuard},
        },
    },
    xwayland::XWaylandState,
};
use anyhow::Context;
use calloop::RegistrationToken;
use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader},
    DesktopLanguageRequester,
};
use rust_embed::RustEmbed;
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Fourcc},
        drm::DrmNode,
        renderer::{
            element::{
                default_primary_scanout_output_compare, utils::select_dmabuf_feedback,
                RenderElementState, RenderElementStates,
            },
            ImportDma,
        },
    },
    desktop::{
        layer_map_for_output,
        utils::{
            send_dmabuf_feedback_surface_tree, send_frames_surface_tree,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            with_surfaces_surface_tree,
        },
        PopupManager,
    },
    input::{pointer::CursorImageStatus, SeatState},
    output::{Output, Scale, WeakOutput},
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities,
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_shm, wl_surface::WlSurface},
            Client, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Monotonic, Point},
    wayland::{
        alpha_modifier::AlphaModifierState,
        compositor::{CompositorClientState, CompositorState, SurfaceData},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufState},
        fractional_scale::{with_fractional_scale, FractionalScaleManagerState},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        input_method::InputMethodManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        seat::WaylandFocus,
        security_context::{SecurityContext, SecurityContextState},
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::XWaylandClientData,
};
use time::UtcOffset;

#[cfg(feature = "systemd")]
use std::os::fd::OwnedFd;

use std::{
    cell::RefCell,
    cmp::min,
    collections::HashSet,
    ffi::OsString,
    process::Child,
    sync::{atomic::AtomicBool, Arc, LazyLock, Once},
    time::{Duration, Instant},
};

#[derive(RustEmbed)]
#[folder = "resources/i18n"]
struct Localizations;

pub static LANG_LOADER: LazyLock<FluentLanguageLoader> =
    LazyLock::new(|| fluent_language_loader!());

#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::state::LANG_LOADER, $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::state::LANG_LOADER, $message_id, $($args), *)
    }};
}

pub struct ClientState {
    pub compositor_client_state: CompositorClientState,
    pub advertised_drm_node: Option<DrmNode>,
    pub privileged: bool,
    pub evls: LoopSignal,
    pub security_context: Option<SecurityContext>,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        self.evls.wakeup();
    }
}

pub fn advertised_node_for_client(client: &Client) -> Option<DrmNode> {
    // Lets check the global drm-node the client got either through default-feedback or wl_drm
    if let Some(normal_client) = client.get_data::<ClientState>() {
        return normal_client.advertised_drm_node.clone();
    }
    // last but not least all xwayland-surfaces should also share a single node
    if let Some(xwayland_client) = client.get_data::<XWaylandClientData>() {
        return xwayland_client.user_data().get::<DrmNode>().cloned();
    }
    None
}

pub fn advertised_node_for_surface(w: &WlSurface, dh: &DisplayHandle) -> Option<DrmNode> {
    let client = dh.get_client(w.id()).ok()?;
    advertised_node_for_client(&client)
}

#[derive(Debug)]
pub enum LastRefresh {
    None,
    At(Instant),
    Scheduled(RegistrationToken),
}

#[derive(Debug)]
pub struct State {
    pub backend: BackendData,
    pub common: Common,
    pub ready: Once,
    pub last_refresh: LastRefresh,
}

#[derive(Debug)]
pub struct Common {
    pub config: Config,

    pub socket: OsString,
    pub display_handle: DisplayHandle,
    pub event_loop_handle: LoopHandle<'static, State>,
    pub event_loop_signal: LoopSignal,

    pub popups: PopupManager,
    pub shell: Arc<parking_lot::RwLock<Shell>>,

    pub clock: Clock<Monotonic>,
    pub startup_done: Arc<AtomicBool>,
    pub should_stop: bool,
    pub local_offset: time::UtcOffset,
    pub gesture_state: Option<GestureState>,

    pub kiosk_child: Option<Child>,
    pub theme: cosmic::Theme,

    // wayland state
    pub compositor_state: CompositorState,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub output_state: OutputManagerState,
    pub output_configuration_state: OutputConfigurationState<State>,
    pub output_power_state: OutputPowerState,
    pub presentation_state: PresentationState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: Option<DataControlState>,
    pub image_capture_source_state: ImageCaptureSourceState,
    pub screencopy_state: ScreencopyState,
    pub seat_state: SeatState<State>,
    pub session_lock_manager_state: SessionLockManagerState,
    pub idle_notifier_state: IdleNotifierState<State>,
    pub idle_inhibit_manager_state: IdleInhibitManagerState,
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    pub shm_state: ShmState,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub wl_drm_state: WlDrmState<Option<DrmNode>>,
    pub viewporter_state: ViewporterState,
    pub kde_decoration_state: KdeDecorationState,
    pub xdg_decoration_state: XdgDecorationState,
    pub overlap_notify_state: OverlapNotifyState,
    pub a11y_state: A11yState,

    // shell-related wayland state
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub toplevel_info_state: ToplevelInfoState<State, CosmicSurface>,
    pub toplevel_management_state: ToplevelManagementState,
    pub xdg_activation_state: XdgActivationState,
    pub xdg_foreign_state: XdgForeignState,
    pub workspace_state: WorkspaceState<State>,
    pub xwayland_scale: Option<f64>,
    pub xwayland_state: Option<XWaylandState>,
    pub xwayland_shell_state: XWaylandShellState,
    pub pointer_focus_state: Option<PointerFocusState>,

    pub atspi_state: AtspiState,
    pub atspi_ei: crate::wayland::handlers::atspi::AtspiEiState,

    #[cfg(feature = "systemd")]
    inhibit_lid_fd: Option<OwnedFd>,
}

#[derive(Debug)]
pub enum BackendData {
    X11(X11State),
    Winit(WinitState),
    Kms(KmsState),
    // TODO
    // Wayland(WaylandState),
    Unset,
}

pub enum LockedBackend<'a> {
    X11(&'a mut X11State),
    Winit(&'a mut WinitState),
    Kms(KmsGuard<'a>),
}

#[derive(Debug, Clone)]
pub struct SurfaceDmabufFeedback {
    pub render_feedback: DmabufFeedback,
    pub scanout_feedback: DmabufFeedback,
    pub primary_scanout_feedback: DmabufFeedback,
}

#[derive(Debug)]
struct SurfaceFrameThrottlingState {
    last_sent_at: RefCell<Option<(WeakOutput, usize)>>,
}
impl Default for SurfaceFrameThrottlingState {
    fn default() -> Self {
        SurfaceFrameThrottlingState {
            last_sent_at: RefCell::new(None),
        }
    }
}

impl BackendData {
    pub fn kms(&mut self) -> &mut KmsState {
        match self {
            BackendData::Kms(kms_state) => kms_state,
            _ => unreachable!("Called kms in non kms backend"),
        }
    }

    pub fn x11(&mut self) -> &mut X11State {
        match self {
            BackendData::X11(x11_state) => x11_state,
            _ => unreachable!("Called x11 in non x11 backend"),
        }
    }

    pub fn winit(&mut self) -> &mut WinitState {
        match self {
            BackendData::Winit(winit_state) => winit_state,
            _ => unreachable!("Called winit in non winit backend"),
        }
    }

    pub fn schedule_render(&mut self, output: &Output) {
        match self {
            BackendData::Winit(_) => {} // We cannot do this on the winit backend.
            // Winit has a very strict render-loop and skipping frames breaks atleast the wayland winit-backend.
            // Swapping with damage (which should be empty on these frames) is likely good enough anyway.
            BackendData::X11(state) => state.schedule_render(output),
            BackendData::Kms(state) => state.schedule_render(output),
            _ => unreachable!("No backend was initialized"),
        }
    }

    pub fn dmabuf_imported(
        &mut self,
        client: Option<Client>,
        global: &DmabufGlobal,
        dmabuf: Dmabuf,
    ) -> Result<Option<DrmNode>, anyhow::Error> {
        match self {
            BackendData::Kms(state) => {
                return state
                    .dmabuf_imported(client, global, dmabuf)
                    .map(|node| Some(node));
            }
            BackendData::Winit(state) => {
                state.backend.renderer().import_dmabuf(&dmabuf, None)?;
            }
            BackendData::X11(state) => {
                state.renderer.import_dmabuf(&dmabuf, None)?;
            }
            _ => unreachable!("No backend set when importing dmabuf"),
        };
        Ok(None)
    }

    /// Get an offscreen renderer for screen capture / screenshot rendering
    ///
    /// `kms_node_cb` callback use used to determine nodes to render with when using kms backend.
    /// If this returns `None`, it will attempt to use llvmpipe, then panic if no renderer is
    /// found.
    pub fn offscreen_renderer<N: Into<KmsNodes>, F: FnOnce(&mut KmsState) -> Option<N>>(
        &mut self,
        kms_node_cb: F,
    ) -> Result<RendererRef, GlMultiError> {
        match self {
            BackendData::Kms(kms) => {
                if let Some(nodes) = kms_node_cb(kms) {
                    let nodes = nodes.into();
                    Ok(RendererRef::GlMulti(kms.api.renderer(
                        &nodes.render_node,
                        &nodes.target_node,
                        nodes.copy_format,
                    )?))
                } else {
                    Ok(RendererRef::Glow(
                        kms.software_renderer
                            .as_mut()
                            .expect("No Software Rendering"),
                    ))
                }
            }
            BackendData::Winit(winit) => Ok(RendererRef::Glow(winit.backend.renderer())),
            BackendData::X11(x11) => Ok(RendererRef::Glow(&mut x11.renderer)),
            _ => unreachable!("No backend set when getting offscreen renderer"),
        }
    }

    pub fn update_screen_filter(&mut self, screen_filter: &ScreenFilter) -> anyhow::Result<()> {
        match self {
            BackendData::Kms(state) => state.update_screen_filter(screen_filter),
            BackendData::Winit(state) => state.update_screen_filter(screen_filter),
            BackendData::X11(state) => state.update_screen_filter(screen_filter),
            _ => unreachable!("No backend set when setting screen filters"),
        }
    }

    pub fn lock(&mut self) -> LockedBackend<'_> {
        match self {
            BackendData::Kms(state) => LockedBackend::Kms(state.lock_devices()),
            BackendData::X11(state) => LockedBackend::X11(state),
            BackendData::Winit(state) => LockedBackend::Winit(state),
            _ => unreachable!("Tried to lock unset backend"),
        }
    }
}

impl<'a> LockedBackend<'a> {
    pub fn all_outputs(&self) -> Vec<Output> {
        match self {
            LockedBackend::Kms(state) => state.all_outputs(),
            LockedBackend::X11(state) => state.all_outputs(),
            LockedBackend::Winit(state) => state.all_outputs(),
        }
    }

    pub fn apply_config_for_outputs(
        &mut self,
        test_only: bool,
        loop_handle: &LoopHandle<'static, State>,
        screen_filter: &ScreenFilter,
        shell: Arc<parking_lot::RwLock<Shell>>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        xdg_activation_state: &XdgActivationState,
        startup_done: Arc<AtomicBool>,
        clock: &Clock<Monotonic>,
    ) -> Result<(), anyhow::Error> {
        let all_outputs = self.all_outputs();

        // update outputs, so that `OutputModeSource`s are correct
        for output in &all_outputs {
            // apply to Output
            let final_config = output
                .user_data()
                .get::<RefCell<OutputConfig>>()
                .unwrap()
                .borrow();

            let mode = Some(final_config.output_mode()).filter(|m| match output.current_mode() {
                None => true,
                Some(c_m) => m.size != c_m.size || m.refresh != c_m.refresh,
            });
            let transform =
                Some(final_config.transform.into()).filter(|x| *x != output.current_transform());
            let scale = Some(final_config.scale)
                .filter(|x| *x != output.current_scale().fractional_scale());
            let location = Some(Point::from((
                final_config.position.0 as i32,
                final_config.position.1 as i32,
            )))
            .filter(|x| *x != output.current_location());
            output.change_current_state(mode, transform, scale.map(Scale::Fractional), location);

            output.set_adaptive_sync(final_config.vrr);
        }

        match self {
            LockedBackend::Kms(state) => state.apply_config_for_outputs(
                test_only,
                loop_handle,
                screen_filter,
                shell.clone(),
                startup_done,
                clock,
            ),
            LockedBackend::Winit(state) => state.apply_config_for_outputs(test_only),
            LockedBackend::X11(state) => state.apply_config_for_outputs(test_only),
        }?;

        let mut shell_ref = shell.write();
        for output in &all_outputs {
            // apply the rest; add / remove outputs
            let final_config = output
                .user_data()
                .get::<RefCell<OutputConfig>>()
                .unwrap()
                .borrow();

            output.set_mirroring(match &final_config.enabled {
                OutputState::Mirroring(conn) => shell_ref
                    .outputs()
                    .find(|output| &output.name() == conn)
                    .cloned(),
                _ => None,
            });

            match final_config.enabled {
                OutputState::Enabled => shell_ref.workspaces.add_output(&output, workspace_state),
                _ => {
                    let shell = &mut *shell_ref;
                    shell.workspaces.remove_output(
                        &output,
                        shell.seats.iter(),
                        workspace_state,
                        xdg_activation_state,
                    )
                }
            }

            layer_map_for_output(&output).arrange();
        }

        // Update layout for changes in resolution, scale, orientation
        shell_ref.workspaces.recalculate();
        let active_outputs = shell_ref.outputs().cloned().collect::<Vec<_>>();
        std::mem::drop(shell_ref);

        for output in active_outputs {
            match self {
                LockedBackend::Winit(_) => {} // We cannot do this on the winit backend.
                // Winit has a very strict render-loop and skipping frames breaks atleast the wayland winit-backend.
                // Swapping with damage (which should be empty on these frames) is likely good enough anyway.
                LockedBackend::X11(state) => state.schedule_render(&output),
                LockedBackend::Kms(state) => state.schedule_render(&output),
            }
        }

        loop_handle.insert_idle(move |state| {
            state.update_inhibitor_locks();
            state.common.update_xwayland_scale();
            state.common.update_xwayland_primary_output();
        });

        Ok(())
    }
}

pub struct KmsNodes {
    pub render_node: DrmNode,
    pub target_node: DrmNode,
    pub copy_format: Fourcc,
}

impl From<DrmNode> for KmsNodes {
    fn from(node: DrmNode) -> Self {
        KmsNodes {
            render_node: node,
            target_node: node,
            // Ignored if render == target
            copy_format: Fourcc::Abgr8888,
        }
    }
}

pub fn client_has_no_security_context(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .map_or(true, |client_state| client_state.security_context.is_none())
}

pub fn client_is_privileged(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .map_or(false, |client_state| client_state.privileged)
}

fn enable_wayland_security() -> bool {
    crate::utils::env::bool_var("COSMIC_ENABLE_WAYLAND_SECURITY").unwrap_or(false)
}

impl State {
    pub fn new(
        dh: &DisplayHandle,
        socket: OsString,
        handle: LoopHandle<'static, State>,
        signal: LoopSignal,
    ) -> State {
        let requested_languages = DesktopLanguageRequester::requested_languages();
        i18n_embed::select(&*LANG_LOADER, &Localizations, &requested_languages)
            .with_context(|| "Failed to load languages")
            .unwrap();

        let local_offset = UtcOffset::current_local_offset().expect("No yet multithreaded");
        let clock = Clock::new();
        let config = Config::load(&handle);
        let compositor_state = CompositorState::new::<Self>(dh);
        let data_device_state = DataDeviceState::new::<Self>(dh);
        let dmabuf_state = DmabufState::new();
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(dh);
        let output_state = OutputManagerState::new_with_xdg_output::<Self>(dh);
        let output_configuration_state =
            OutputConfigurationState::new(dh, handle.clone(), client_is_privileged);
        let output_power_state = OutputPowerState::new::<Self, _>(dh, client_is_privileged);
        let overlap_notify_state =
            OverlapNotifyState::new::<Self, _>(dh, client_has_no_security_context);
        let presentation_state = PresentationState::new::<Self>(dh, clock.id() as u32);
        let primary_selection_state = PrimarySelectionState::new::<Self>(dh);
        let image_capture_source_state =
            ImageCaptureSourceState::new::<Self, _>(dh, client_is_privileged);
        let screencopy_state = ScreencopyState::new::<Self, _>(dh, client_is_privileged);
        let shm_state =
            ShmState::new::<Self>(dh, vec![wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888]);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<State>(dh);
        let seat_state = SeatState::<Self>::new();
        let viewporter_state = ViewporterState::new::<Self>(dh);
        let wl_drm_state = WlDrmState::<Option<DrmNode>>::default();
        let kde_decoration_state = KdeDecorationState::new::<Self>(&dh, Mode::Client);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let session_lock_manager_state =
            SessionLockManagerState::new::<Self, _>(&dh, client_is_privileged);
        XWaylandKeyboardGrabState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        PointerConstraintsState::new::<Self>(&dh);
        PointerGesturesState::new::<Self>(&dh);
        TabletManagerState::new::<Self>(&dh);
        SecurityContextState::new::<Self, _>(&dh, client_has_no_security_context);
        InputMethodManagerState::new::<Self, _>(&dh, client_is_privileged);
        TextInputManagerState::new::<Self>(&dh);
        VirtualKeyboardManagerState::new::<State, _>(&dh, client_is_privileged);
        AlphaModifierState::new::<Self>(&dh);
        SinglePixelBufferState::new::<Self>(&dh);

        let idle_notifier_state = IdleNotifierState::<Self>::new(&dh, handle.clone());
        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<State>(&dh);
        let idle_inhibiting_surfaces = HashSet::new();

        let data_control_state = crate::utils::env::bool_var("COSMIC_DATA_CONTROL_ENABLED")
            .unwrap_or(false)
            .then(|| {
                DataControlState::new::<Self, _>(dh, Some(&primary_selection_state), |_| true)
            });

        let shell = Arc::new(parking_lot::RwLock::new(Shell::new(&config)));

        let layer_shell_state =
            WlrLayerShellState::new_with_filter::<State, _>(dh, client_is_privileged);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<State>(
            dh,
            [
                WmCapabilities::Fullscreen,
                WmCapabilities::Maximize,
                WmCapabilities::Minimize,
                WmCapabilities::WindowMenu,
            ],
        );
        let xdg_activation_state = XdgActivationState::new::<State>(dh);
        let xdg_foreign_state = XdgForeignState::new::<State>(dh);
        let toplevel_info_state = ToplevelInfoState::new(dh, client_is_privileged);
        let toplevel_management_state = ToplevelManagementState::new::<State, _>(
            dh,
            vec![
                ManagementCapabilities::Close,
                ManagementCapabilities::Activate,
                ManagementCapabilities::Maximize,
                ManagementCapabilities::Minimize,
                ManagementCapabilities::MoveToWorkspace,
            ],
            client_is_privileged,
        );
        let workspace_state = WorkspaceState::new(dh, client_is_privileged);

        if let Err(err) = crate::dbus::init(&handle) {
            tracing::warn!(?err, "Failed to initialize dbus handlers");
        }

        let a11y_state = A11yState::new::<State, _>(dh, client_is_privileged);

        // TODO: Restrict to only specific client?
        let atspi_state = AtspiState::new::<State, _>(dh, |_| true);

        State {
            common: Common {
                config,
                socket,
                display_handle: dh.clone(),
                event_loop_handle: handle,
                event_loop_signal: signal,

                popups: PopupManager::default(),
                shell,

                local_offset,

                clock,
                startup_done: Arc::new(AtomicBool::new(false)),
                should_stop: false,
                gesture_state: None,

                kiosk_child: None,
                theme: cosmic::theme::system_preference(),

                compositor_state,
                data_device_state,
                dmabuf_state,
                fractional_scale_state,
                idle_notifier_state,
                idle_inhibit_manager_state,
                idle_inhibiting_surfaces,
                image_capture_source_state,
                screencopy_state,
                shm_state,
                cursor_shape_manager_state,
                seat_state,
                session_lock_manager_state,
                keyboard_shortcuts_inhibit_state,
                output_state,
                output_configuration_state,
                output_power_state,
                overlap_notify_state,
                presentation_state,
                primary_selection_state,
                data_control_state,
                viewporter_state,
                wl_drm_state,
                kde_decoration_state,
                xdg_decoration_state,
                xdg_shell_state,
                layer_shell_state,
                toplevel_info_state,
                toplevel_management_state,
                xdg_activation_state,
                xdg_foreign_state,
                workspace_state,
                a11y_state,
                xwayland_scale: None,
                xwayland_state: None,
                xwayland_shell_state,
                pointer_focus_state: None,

                atspi_state,
                atspi_ei: Default::default(),

                #[cfg(feature = "systemd")]
                inhibit_lid_fd: None,
            },
            backend: BackendData::Unset,
            ready: Once::new(),
            last_refresh: LastRefresh::None,
        }
    }

    pub fn new_client_state(&self) -> ClientState {
        ClientState {
            compositor_client_state: CompositorClientState::default(),
            advertised_drm_node: match &self.backend {
                BackendData::Kms(kms_state) => *kms_state.primary_node.read().unwrap(),
                _ => None,
            },
            privileged: !enable_wayland_security(),
            evls: self.common.event_loop_signal.clone(),
            security_context: None,
        }
    }

    fn update_inhibitor_locks(&mut self) {
        #[cfg(feature = "systemd")]
        {
            use tracing::{debug, error};

            let outputs = self.backend.lock().all_outputs();
            let should_handle_lid = outputs.iter().any(|o| o.is_internal()) && outputs.len() >= 2;

            if should_handle_lid {
                if self.common.inhibit_lid_fd.is_none() {
                    match crate::dbus::logind::inhibit_lid() {
                        Ok(fd) => {
                            debug!("Inhibiting lid switch");
                            self.common.inhibit_lid_fd = Some(fd);
                        }
                        Err(err) => {
                            error!("Failed to inhibit lid switch: {}", err);
                        }
                    }
                }
            } else {
                if self.common.inhibit_lid_fd.take().is_some() {
                    debug!("Removing inhibitor-lock on lid switch")
                }
            }
        }
    }
}

fn primary_scanout_output_compare<'a>(
    current_output: &'a Output,
    current_state: &RenderElementState,
    next_output: &'a Output,
    next_state: &RenderElementState,
) -> &'a Output {
    if !crate::wayland::protocols::output_configuration::head_is_enabled(current_output) {
        return next_output;
    }

    default_primary_scanout_output_compare(current_output, current_state, next_output, next_state)
}

impl Common {
    #[profiling::function]
    pub fn update_primary_output(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) {
        let shell = self.shell.read();
        let processor = |surface: &WlSurface, states: &SurfaceData| {
            let primary_scanout_output = update_surface_primary_scanout_output(
                surface,
                output,
                states,
                render_element_states,
                primary_scanout_output_compare,
            );
            if let Some(output) = primary_scanout_output {
                with_fractional_scale(states, |fraction_scale| {
                    fraction_scale.set_preferred_scale(output.current_scale().fractional_scale());
                });
            }
        };

        // lock surface
        if let Some(session_lock) = shell.session_lock.as_ref() {
            if let Some(lock_surface) = session_lock.surfaces.get(output) {
                with_surfaces_surface_tree(lock_surface.wl_surface(), processor)
            }
        }

        for seat in shell
            .seats
            .iter()
            .filter(|seat| &seat.active_output() == output)
        {
            let cursor_status = seat.cursor_image_status();

            // cursor ...
            if let CursorImageStatus::Surface(wl_surface) = cursor_status {
                with_surfaces_surface_tree(&wl_surface, processor);
            }

            // grabs
            if let Some(move_grab) = seat.user_data().get::<SeatMoveGrabState>() {
                if let Some(grab_state) = move_grab.lock().unwrap().as_ref() {
                    for (window, _) in grab_state.element().windows() {
                        window.with_surfaces(processor);
                    }
                }
            }

            if let Some(icon) = get_dnd_icon(seat) {
                with_surfaces_surface_tree(&icon.surface, processor);
            }
        }

        // sticky window
        for set in shell.workspaces.sets.values() {
            set.sticky_layer.mapped().for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    window.with_surfaces(processor);
                }
            });
        }

        // normal windows
        for space in shell.workspaces.spaces() {
            if let Some(window) = space.get_fullscreen() {
                window.with_surfaces(processor);
            }
            space.mapped().for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    window.with_surfaces(processor);
                }
            });
            space.minimized_windows.iter().for_each(|m| {
                for window in m.windows() {
                    window.with_surfaces(processor);
                }
            })
        }

        // OR windows
        shell.override_redirect_windows.iter().for_each(|or| {
            if let Some(wl_surface) = or.wl_surface() {
                with_surfaces_surface_tree(&wl_surface, processor);
            }
        });

        // layer surfaces
        for o in shell.outputs() {
            let map = smithay::desktop::layer_map_for_output(o);
            for layer_surface in map.layers() {
                layer_surface.with_surfaces(processor);
            }
        }
    }

    #[profiling::function]
    pub fn send_dmabuf_feedback(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
        mut dmabuf_feedback: impl FnMut(DrmNode) -> Option<SurfaceDmabufFeedback>,
    ) {
        let shell = self.shell.read();

        if let Some(session_lock) = shell.session_lock.as_ref() {
            if let Some(lock_surface) = session_lock.surfaces.get(output) {
                if let Some(feedback) =
                    advertised_node_for_surface(lock_surface.wl_surface(), &self.display_handle)
                        .and_then(|source| dmabuf_feedback(source))
                {
                    send_dmabuf_feedback_surface_tree(
                        &lock_surface.wl_surface(),
                        output,
                        surface_primary_scanout_output,
                        |surface, _| {
                            select_dmabuf_feedback(
                                surface,
                                render_element_states,
                                &feedback.render_feedback,
                                &feedback.primary_scanout_feedback,
                            )
                        },
                    )
                }
            }
        }

        for seat in shell
            .seats
            .iter()
            .filter(|seat| &seat.active_output() == output)
        {
            let cursor_status = seat.cursor_image_status();

            if let CursorImageStatus::Surface(wl_surface) = cursor_status {
                if let Some(feedback) =
                    advertised_node_for_surface(&wl_surface, &self.display_handle)
                        .and_then(|source| dmabuf_feedback(source))
                {
                    send_dmabuf_feedback_surface_tree(
                        &wl_surface,
                        output,
                        surface_primary_scanout_output,
                        |surface, _| {
                            select_dmabuf_feedback(
                                surface,
                                render_element_states,
                                &feedback.render_feedback,
                                &feedback.scanout_feedback,
                            )
                        },
                    );
                }
            }

            if let Some(icon) = get_dnd_icon(seat) {
                if let Some(feedback) =
                    advertised_node_for_surface(&icon.surface, &self.display_handle)
                        .and_then(|source| dmabuf_feedback(source))
                {
                    send_dmabuf_feedback_surface_tree(
                        &icon.surface,
                        output,
                        surface_primary_scanout_output,
                        |surface, _| {
                            select_dmabuf_feedback(
                                surface,
                                render_element_states,
                                &feedback.render_feedback,
                                &feedback.scanout_feedback,
                            )
                        },
                    );
                }
            }

            if let Some(move_grab) = seat.user_data().get::<SeatMoveGrabState>() {
                if let Some(grab_state) = move_grab.lock().unwrap().as_ref() {
                    for (window, _) in grab_state.element().windows() {
                        if let Some(feedback) = window
                            .wl_surface()
                            .and_then(|wl_surface| {
                                advertised_node_for_surface(&wl_surface, &self.display_handle)
                            })
                            .and_then(|source| dmabuf_feedback(source))
                        {
                            window.send_dmabuf_feedback(
                                output,
                                &feedback,
                                render_element_states,
                                surface_primary_scanout_output,
                            );
                        }
                    }
                }
            }
        }

        shell
            .workspaces
            .sets
            .get(output)
            .unwrap()
            .sticky_layer
            .mapped()
            .for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    if let Some(feedback) = window
                        .wl_surface()
                        .and_then(|wl_surface| {
                            advertised_node_for_surface(&wl_surface, &self.display_handle)
                        })
                        .and_then(|source| dmabuf_feedback(source))
                    {
                        window.send_dmabuf_feedback(
                            output,
                            &feedback,
                            render_element_states,
                            surface_primary_scanout_output,
                        );
                    }
                }
            });

        if let Some(active) = shell.active_space(output) {
            if let Some(window) = active.get_fullscreen() {
                if let Some(feedback) = window
                    .wl_surface()
                    .and_then(|wl_surface| {
                        advertised_node_for_surface(&wl_surface, &self.display_handle)
                    })
                    .and_then(|source| dmabuf_feedback(source))
                {
                    window.send_dmabuf_feedback(
                        output,
                        &feedback,
                        render_element_states,
                        surface_primary_scanout_output,
                    );
                }
            }
            active.mapped().for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    if let Some(feedback) = window
                        .wl_surface()
                        .and_then(|wl_surface| {
                            advertised_node_for_surface(&wl_surface, &self.display_handle)
                        })
                        .and_then(|source| dmabuf_feedback(source))
                    {
                        window.send_dmabuf_feedback(
                            output,
                            &feedback,
                            render_element_states,
                            surface_primary_scanout_output,
                        );
                    }
                }
            });
        }

        shell.override_redirect_windows.iter().for_each(|or| {
            if let Some(wl_surface) = or.wl_surface() {
                if let Some(feedback) =
                    advertised_node_for_surface(&wl_surface, &self.display_handle)
                        .and_then(|source| dmabuf_feedback(source))
                {
                    send_dmabuf_feedback_surface_tree(
                        &wl_surface,
                        output,
                        surface_primary_scanout_output,
                        |surface, _| {
                            select_dmabuf_feedback(
                                surface,
                                render_element_states,
                                &feedback.render_feedback,
                                &feedback.scanout_feedback,
                            )
                        },
                    )
                }
            }
        });

        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers() {
            if let Some(feedback) =
                advertised_node_for_surface(layer_surface.wl_surface(), &self.display_handle)
                    .and_then(|source| dmabuf_feedback(source))
            {
                layer_surface.send_dmabuf_feedback(
                    output,
                    surface_primary_scanout_output,
                    |surface, _| {
                        select_dmabuf_feedback(
                            surface,
                            render_element_states,
                            &feedback.render_feedback,
                            &feedback.scanout_feedback,
                        )
                    },
                );
            }
        }
    }

    #[profiling::function]
    pub fn send_frames(&self, output: &Output, sequence: Option<usize>) {
        let time = self.clock.now();
        let should_send = |surface: &WlSurface, states: &SurfaceData| {
            // Do the standard primary scanout output check. For pointer surfaces it deduplicates
            // the frame callbacks across potentially multiple outputs, and for regular windows and
            // layer-shell surfaces it avoids sending frame callbacks to invisible surfaces.
            let current_primary_output = surface_primary_scanout_output(surface, states);
            if current_primary_output.as_ref() != Some(output) {
                return None;
            }

            let Some(sequence) = sequence else {
                return Some(output.clone());
            };

            // Next, check the throttling status.
            let frame_throttling_state = states
                .data_map
                .get_or_insert(SurfaceFrameThrottlingState::default);
            let mut last_sent_at = frame_throttling_state.last_sent_at.borrow_mut();

            let mut send = true;

            // If we already sent a frame callback to this surface this output refresh
            // cycle, don't send one again to prevent empty-damage commit busy loops.
            if let Some((last_output, last_sequence)) = &*last_sent_at {
                if last_output == output && *last_sequence == sequence {
                    send = false;
                }
            }

            if send {
                *last_sent_at = Some((output.downgrade(), sequence));
                Some(output.clone())
            } else {
                None
            }
        };
        const THROTTLE: Option<Duration> = Some(Duration::from_millis(995));
        const SCREENCOPY_THROTTLE: Option<Duration> = Some(Duration::from_nanos(16_666_666));

        fn throttle(session_holder: &impl SessionHolder) -> Option<Duration> {
            if session_holder.sessions().is_empty() && session_holder.cursor_sessions().is_empty() {
                THROTTLE
            } else {
                SCREENCOPY_THROTTLE
            }
        }

        let shell = self.shell.read();

        if let Some(session_lock) = shell.session_lock.as_ref() {
            if let Some(lock_surface) = session_lock.surfaces.get(output) {
                send_frames_surface_tree(
                    lock_surface.wl_surface(),
                    output,
                    time,
                    None,
                    should_send,
                );
            }
        }

        for seat in shell
            .seats
            .iter()
            .filter(|seat| &seat.active_output() == output)
        {
            let cursor_status = seat.cursor_image_status();

            if let CursorImageStatus::Surface(wl_surface) = cursor_status {
                send_frames_surface_tree(
                    &wl_surface,
                    output,
                    time,
                    Some(Duration::ZERO),
                    should_send,
                )
            }

            if let Some(move_grab) = seat.user_data().get::<SeatMoveGrabState>() {
                if let Some(grab_state) = move_grab.lock().unwrap().as_ref() {
                    for (window, _) in grab_state.element().windows() {
                        window.send_frame(output, time, throttle(&window), should_send);
                    }
                }
            }

            if let Some(icon) = get_dnd_icon(seat) {
                send_frames_surface_tree(
                    &icon.surface,
                    output,
                    time,
                    Some(Duration::ZERO),
                    should_send,
                )
            }
        }

        shell
            .workspaces
            .sets
            .get(output)
            .unwrap()
            .sticky_layer
            .mapped()
            .for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    window.send_frame(output, time, throttle(&window), should_send);
                }
            });

        if let Some(active) = shell.active_space(output) {
            if let Some(window) = active.get_fullscreen() {
                window.send_frame(output, time, throttle(window), should_send);
            }
            active.mapped().for_each(|mapped| {
                for (window, _) in mapped.windows() {
                    window.send_frame(output, time, throttle(&window), should_send);
                }
            });

            // other (throttled) windows
            active.minimized_windows.iter().for_each(|m| {
                for window in m.windows() {
                    window.send_frame(output, time, throttle(&window), |_, _| None);
                }
            });

            for space in shell
                .workspaces
                .spaces_for_output(output)
                .filter(|w| w.handle != active.handle)
            {
                if let Some(window) = space.get_fullscreen() {
                    let throttle = min(throttle(space), throttle(window));
                    window.send_frame(output, time, throttle, |_, _| None);
                }
                space.mapped().for_each(|mapped| {
                    for (window, _) in mapped.windows() {
                        let throttle = min(throttle(space), throttle(&window));
                        window.send_frame(output, time, throttle, |_, _| None);
                    }
                });
                space.minimized_windows.iter().for_each(|m| {
                    for window in m.windows() {
                        window.send_frame(output, time, throttle(&window), |_, _| None);
                    }
                })
            }
        }

        shell.override_redirect_windows.iter().for_each(|or| {
            if let Some(wl_surface) = or.wl_surface() {
                send_frames_surface_tree(&wl_surface, output, time, THROTTLE, should_send);
            }
        });

        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers() {
            layer_surface.send_frame(output, time, THROTTLE, should_send);
        }
    }
}
