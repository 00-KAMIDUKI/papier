use std::{env::var_os, ffi::CStr, io::Cursor, os::fd::AsFd as _, path::PathBuf, time::Duration};

use anyhow::Context as _;
use image::{DynamicImage, GenericImageView, codecs::jpeg::JpegDecoder};
use rustix::{
    fs::{Mode, OFlags},
    mm::{MapFlags, ProtFlags},
};
use tokio::{io::unix::AsyncFd, sync::mpsc};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::{self, WlSurface},
    },
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

fn runtime_dir() -> PathBuf {
    var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = rustix::process::getuid();
            PathBuf::from(format!("/run/user/{uid}"))
        })
}

struct Client {
    globals: Globals,
    window: Option<Window>,
    pointer: Option<WlPointer>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    cache_path: PathBuf,
    cache_size: [u32; 2],
}

impl Client {
    fn new() -> Self {
        let cache_dir = runtime_dir().join("papier");
        Self {
            globals: Globals::default(),
            window: None,
            pointer: None,
            cursor_shape_device: None,
            cache_path: cache_dir,
            cache_size: [0; 2],
        }
    }
}

struct Window {
    surface: WlSurface,
    pixmap: Pixmap,
    buffer: Option<WlBuffer>,
}
impl Window {
    fn new(
        compositor: &WlCompositor,
        layer_shell: &ZwlrLayerShellV1,
        qh: &QueueHandle<Client>,
    ) -> Self {
        let surface = compositor.create_surface(qh, ());
        let layer_surface =
            layer_shell.get_layer_surface(&surface, None, Layer::Bottom, "ice_bar".into(), qh, ());
        layer_surface.set_anchor(Anchor::all());
        surface.commit();

        Window {
            surface,
            pixmap: Pixmap::default(),
            buffer: None,
        }
    }
    fn configure(&mut self, width: u32, height: u32, shm: &WlShm, qh: &QueueHandle<Client>) {
        self.pixmap.unmap();
        self.pixmap.width = width;
        self.pixmap.height = height;
        self.allocate_buffer(shm, qh);
    }
    fn scale(&mut self, scale: u32, shm: &WlShm, qh: &QueueHandle<Client>) {
        self.surface.set_buffer_scale(scale as _);
        self.pixmap.unmap();
        if scale == self.pixmap.scale {
            return;
        }
        self.pixmap.scale = scale;
        self.allocate_buffer(shm, qh);
    }
    fn allocate_buffer(&mut self, shm: &WlShm, qh: &QueueHandle<Client>) {
        const PATH: &CStr = c"/dev/shm/bar";
        let fd = rustix::fs::open(
            PATH,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();

        rustix::fs::ftruncate(&fd, self.pixmap.byte_size() as _).unwrap();
        let data = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                self.pixmap.byte_size() as _,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .unwrap()
        };
        let data = NonNull::new(data as _);
        rustix::fs::unlink(PATH).unwrap();

        let pool = shm.create_pool(fd.as_fd(), self.pixmap.byte_size() as _, qh, ());
        let buffer = pool.create_buffer(
            0,
            self.pixmap.buffer_width() as _,
            self.pixmap.buffer_height() as _,
            self.pixmap.buffer_stride() as _,
            Format::Xrgb8888,
            qh,
            (),
        );
        pool.destroy();

        self.pixmap.data = data;
        self.buffer.replace(buffer).as_ref().map(WlBuffer::destroy);
    }

    fn render(&self, source: Pixmap) {
        for y in 0..self.pixmap.buffer_height() {
            for x in 0..self.pixmap.buffer_width() {
                let dest = self.pixmap.pixel_mut(x, y);
                let x = (x * self.pixmap.buffer_width() / source.width).min(source.width - 1);
                let y = (y * self.pixmap.buffer_height() / source.height).min(source.height - 1);
                *dest = source.pixel(x, y);
            }
        }
        source.unmap();
        self.surface
            .attach(Some(self.buffer.as_ref().unwrap()), 0, 0);
        self.surface.damage_buffer(
            0,
            0,
            self.pixmap.buffer_width() as _,
            self.pixmap.buffer_height() as _,
        );
        self.surface.commit();
    }
}

impl Client {
    async fn fetch(&mut self) -> anyhow::Result<Pixmap> {
        let bytes = reqwest::get("https://bing.biturl.top?format=image&resolution=UHD&mkt=random")
            .await
            .context("Failed to reload image")?
            .bytes()
            .await
            .context("Failed to read image bytes")?;
        let reader = Cursor::new(bytes);
        let decoder = JpegDecoder::new(reader).context("Failed to create decoder")?;
        let img = DynamicImage::from_decoder(decoder).context("Failed to decode image")?;

        self.cache_size = [img.width(), img.height()];
        let len = img.width() as usize * img.height() as usize * 4;
        let fd = rustix::fs::open(
            &self.cache_path,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_raw_mode(0o600),
        )?;
        rustix::fs::ftruncate(&fd, len as _)?;
        let data = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
                &fd,
                0,
            )?
        };
        let data = NonNull::new(data as _);
        let pixmap = Pixmap {
            width: img.width(),
            height: img.height(),
            scale: 1,
            data,
        };
        for y in 0..img.height() {
            for x in 0..img.width() {
                let [r, g, b, _] = img.get_pixel(x, y).0.map(|x| x as u32);
                *pixmap.pixel_mut(x, y) = (r << 16) | (g << 8) | b;
            }
        }
        rustix::fs::fsync(&fd)?;
        Ok(pixmap)
    }
    fn do_reload(&self) -> anyhow::Result<()> {
        let fd = rustix::fs::open(&self.cache_path, OFlags::RDONLY, Mode::from_raw_mode(0o600))?;
        let [width, height] = self.cache_size;
        let len = width as usize * height as usize * 4;
        rustix::fs::ftruncate(&fd, len as _)?;
        let data = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &fd,
                0,
            )?
        };
        let data = NonNull::new(data as _);
        let pixmap = Pixmap {
            width,
            height,
            scale: 1,
            data,
        };
        self.window.as_ref().unwrap().render(pixmap);
        Ok(())
    }
    fn reload(&self) {
        if let Err(err) = self.do_reload() {
            log::error!("{:?}", err);
        }
    }
    async fn tick(&mut self) {
        match self.fetch().await {
            Ok(image) => self.window.as_ref().unwrap().render(image),
            Err(err) => log::error!("{:?}", err),
        };
    }
    fn run(&mut self) -> impl Future<Output = ()> {
        let connection = Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let display = connection.display();
        display.get_registry(&qh, ());
        queue.roundtrip(self).unwrap();

        if let Some(pointer) = self.pointer.as_ref() {
            self.cursor_shape_device = Some(self.globals.cursor_shaper_manager().get_pointer(
                pointer,
                &qh,
                (),
            ));
            self.globals.cursor_shaper_manager().destroy();
        }

        self.window = Some(Window::new(
            self.globals.compositor(),
            self.globals.layer_shell_v1(),
            &qh,
        ));
        self.globals.layer_shell_v1().destroy();
        queue.blocking_dispatch(self).unwrap();

        let (tick_sender, mut tick) = mpsc::channel(1);

        let ping = async move {
            loop {
                tick_sender.send(()).await.unwrap();
                tokio::time::sleep(Duration::from_hours(24)).await;
            }
        };
        let consumer = async move {
            loop {
                let dispatched = queue.dispatch_pending(self).unwrap();
                if dispatched > 0 {
                    continue;
                }

                connection.flush().unwrap();

                let wayland = async {
                    if let Some(guard) = connection.prepare_read() {
                        let _ = AsyncFd::new(guard.connection_fd())
                            .unwrap()
                            .readable()
                            .await
                            .unwrap();
                        _ = guard.read();
                    }
                };

                tokio::select! {
                    _ = tick.recv() => {
                        self.tick().await;
                    },
                    _ = wayland => {
                        queue.dispatch_pending(self).unwrap();
                    },
                }
            }
        };
        async {
            tokio::join!(consumer, ping);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Client {
    fn event(
        client: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                client
                    .window
                    .as_mut()
                    .unwrap()
                    .configure(width, height, client.globals.shm(), qh);
                client.reload();
            }
            zwlr_layer_surface_v1::Event::Closed => {}
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, ()> for Client {
    fn event(
        client: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if client.globals.bind(registry, name, &interface, version, qh) {
                    return;
                }
                match interface.as_str() {
                    "wl_seat" => {
                        let _: WlSeat = registry.bind(name, version, qh, ());
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for Client {
    fn event(
        client: &mut Self,
        _: &WlSurface,
        event: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::PreferredBufferScale { factor } => {
                client
                    .window
                    .as_mut()
                    .unwrap()
                    .scale(factor as _, client.globals.shm(), qh);
                client.reload();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Client {
    fn event(
        client: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                if let WEnum::Value(capabilities) = capabilities
                    && capabilities.contains(Capability::Pointer)
                {
                    client.pointer = Some(seat.get_pointer(qh, ()));
                    seat.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for Client {
    fn event(
        client: &mut Self,
        _: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                client
                    .cursor_shape_device
                    .as_ref()
                    .unwrap()
                    .set_shape(serial, Shape::Pointer);
            }
            _ => todo!(),
        }
    }
}

delegate_noop!(Client: ignore WlCompositor);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ignore WpCursorShapeManagerV1);
delegate_noop!(Client: ignore WpCursorShapeDeviceV1);
delegate_noop!(Client: ignore ZwlrLayerShellV1);

macro_rules! use_globals {
    (get_version inherit, $version:expr) => { $version };
    (get_version $version: expr, $($dummy:expr)?) => { $version };
    ($(@$interface: ident ($version: tt) $vis: vis $name: ident: $type: ty),* $(,)?) => {
        #[derive(Default)]
        struct Globals {
            $($name: Option<$type>),*
        }
        impl Globals {
            fn bind<T>(
                &mut self,
                registry: &WlRegistry,
                name: u32,
                interface: &str,
                version: u32,
                qhandle: &wayland_client::QueueHandle<T>,
            ) -> bool
            where
                T: $(Dispatch<$type, ()> + 'static+)*
            {
                match interface {
                    $(stringify!($interface) => {
                        self.$name = Some(registry.bind(name, use_globals!(get_version $version, version), qhandle, ()));
                        true
                    })*
                    _ => false,
                }
            }

            $($vis fn $name(&self) -> &$type {
                self.$name.as_ref().expect(stringify!($name))
            })*
        }
    };
}

use_globals! {
    @wl_compositor(inherit)
    compositor: WlCompositor,

    @wl_shm(inherit)
    shm: WlShm,

    @zwlr_layer_shell_v1(inherit)
    layer_shell_v1: ZwlrLayerShellV1,

    @wp_cursor_shape_manager_v1(inherit)
    cursor_shaper_manager: WpCursorShapeManagerV1,
}

use std::ptr::NonNull;

#[derive(Clone, Copy)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Option<NonNull<u32>>,
    scale: u32,
}

impl Default for Pixmap {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            data: None,
            scale: 1,
        }
    }
}

impl Pixmap {
    fn buffer_width(&self) -> u32 {
        self.width * self.scale
    }
    fn buffer_height(&self) -> u32 {
        self.height * self.scale
    }
    fn buffer_stride(&self) -> u32 {
        self.buffer_width() * 4
    }
    fn byte_size(&self) -> u32 {
        self.buffer_stride() * self.buffer_height()
    }
    pub fn data(&self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.data.unwrap().as_ptr() as _, self.byte_size() as _)
        }
    }
    fn pixels_mut(&self) -> &mut [u32] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data.unwrap().as_ptr() as _,
                (self.width * self.height * self.scale * self.scale) as _,
            )
        }
    }
    fn unmap(&self) {
        if let Some(ptr) = self.data {
            unsafe { rustix::mm::munmap(ptr.as_ptr() as _, self.byte_size() as _).unwrap() };
        }
    }
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels_mut()[(y * self.buffer_width() + x) as usize]
    }
    pub fn pixel_mut(&self, x: u32, y: u32) -> &mut u32 {
        &mut self.pixels_mut()[(y * self.buffer_width() + x) as usize]
    }
}

fn main() {
    let mut client = Client::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(client.run());
}
