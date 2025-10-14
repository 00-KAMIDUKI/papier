use std::{
    env,
    ffi::CStr,
    fs::{self, File},
    io::{BufReader, Cursor},
    os::fd::AsFd as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use bytes::Bytes;
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

const APP_NAME: &str = "papier";

struct Client {
    cache_path: PathBuf,
    globals: Globals,
    window: Option<Window>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    source: Option<DynamicImage>,
}

fn cache_path() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap();
            PathBuf::from(home).join(".cache")
        })
        .join(APP_NAME)
        .join("cache")
}

fn open_cache(path: impl AsRef<Path>) -> anyhow::Result<DynamicImage> {
    let reader = BufReader::new(File::open(path)?);
    let decoder = JpegDecoder::new(reader)?;
    let img = DynamicImage::from_decoder(decoder)?;
    Ok(img)
}

impl Client {
    fn new() -> Self {
        let cache_path = cache_path();
        let source = open_cache(&cache_path).ok();
        Self {
            cache_path,
            globals: Globals::default(),
            window: None,
            cursor_shape_device: None,
            source,
        }
    }
    fn scale(&mut self, scale: u32, qh: &QueueHandle<Self>) {
        let window = self.window.as_mut().unwrap();
        if scale == window.pixmap.scale {
            return;
        }
        window.surface.set_buffer_scale(scale as _);
        window.pixmap.unmap();
        window.pixmap.scale = scale;
        window.allocate_buffer(self.globals.shm(), qh);
        self.reload();
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
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Background,
            APP_NAME.into(),
            qh,
            (),
        );
        layer_surface.set_exclusive_zone(-1);
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

    fn render(&self, source: &Option<DynamicImage>) {
        if let Some(source) = source {
            for y in 0..self.pixmap.buffer_height() {
                for x in 0..self.pixmap.buffer_width() {
                    let dest = self.pixmap.pixel_mut(x, y);
                    let x = x * source.width() / self.pixmap.buffer_width();
                    let y = y * source.height() / self.pixmap.buffer_height();
                    let [r, g, b, _] = source.get_pixel(x, y).0.map(|x| x as u32);
                    *dest = (r << 16) | (g << 8) | b;
                }
            }
        } else {
            for pixel in self.pixmap.pixels_mut() {
                *pixel = 0x93a7c9;
            }
        }
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

async fn try_fetch() -> anyhow::Result<Bytes> {
    reqwest::get("https://bing.biturl.top?format=image&resolution=UHD&mkt=random")
        .await
        .context("Failed to reload image")?
        .bytes()
        .await
        .context("Failed to read image bytes")
}

fn decode_image(bytes: Bytes) -> anyhow::Result<DynamicImage> {
    let reader = Cursor::new(bytes);
    let decoder = JpegDecoder::new(reader).context("Failed to create decoder")?;
    let img = DynamicImage::from_decoder(decoder).context("Failed to decode image")?;
    Ok(img)
}

impl Client {
    fn fetch(&self, retry: usize) -> impl Future<Output = anyhow::Result<DynamicImage>> {
        Box::pin(async move {
            if retry == 0 {
                anyhow::bail!("failed to fetch image");
            }
            match try_fetch().await {
                Ok(bytes) => match decode_image(bytes.clone()) {
                    Ok(img) => {
                        fs::create_dir_all(self.cache_path.parent().unwrap())
                            .and_then(|_| fs::write(&self.cache_path, bytes))
                            .inspect_err(|e| log::warn!("failed to cache image: {e:?}"))
                            .ok();
                        Ok(img)
                    }
                    Err(_) => self.fetch(retry - 1).await,
                },
                Err(e) => {
                    log::warn!("{e:?}");
                    tokio::time::sleep(Duration::new(1, 0)).await;
                    self.fetch(retry - 1).await
                }
            }
        })
    }
    fn reload(&self) {
        self.window.as_ref().unwrap().render(&self.source);
    }
    async fn tick(&mut self) {
        match self.fetch(10).await {
            Ok(image) => {
                self.source = Some(image);
                self.reload();
            }
            Err(err) => log::error!("{err:?}"),
        };
    }
    fn run(&mut self) -> impl Future<Output = ()> {
        let connection = Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let display = connection.display();
        display.get_registry(&qh, ());
        queue.roundtrip(self).unwrap();

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
                client.scale(factor as _, qh);
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
                    let pointer = seat.get_pointer(qh, ());

                    client.cursor_shape_device = Some(
                        client
                            .globals
                            .cursor_shaper_manager()
                            .get_pointer(&pointer, &qh, ()),
                    );
                    client.globals.cursor_shaper_manager().destroy();
                    // seat.release();
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
                    .set_shape(serial, Shape::Default);
            }
            _ => {}
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

#[derive(Debug, Clone, Copy)]
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
    fn pixels_mut(&self) -> &mut [u32] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data.unwrap().as_ptr() as _,
                (self.buffer_width() * self.buffer_height()) as _,
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
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();
    let mut client = Client::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(client.run());
}
