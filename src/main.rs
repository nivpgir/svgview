use anyhow::Result;
use pixels::{Pixels, SurfaceTexture};

use std::io::Read;
use std::path::{Path, PathBuf};
use winit::dpi::PhysicalSize;
use winit::event::{Event, VirtualKeyCode};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowBuilder;
use winit_input_helper::WinitInputHelper;

use log::warn;
use notify::{Op, ReadDirectoryChangesWatcher, RecursiveMode, Watcher, raw_watcher};
use pixels::wgpu::Color;
use std::sync::mpsc::channel;
use std::thread;

use tiny_skia::Pixmap;
use usvg::{Options, Tree};

struct State {
    file: Option<PathBuf>,
    _watcher: Option<ReadDirectoryChangesWatcher>,
    options: Options,
    pixels: Pixmap,
    svg_data: Tree,

    width: u32,
    height: u32,
}

fn main() -> Result<()> {
    // INFRA
    pretty_env_logger::init();

    // CLI
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 {
        println!("Usage:\n\tsvgview <path-to-svg>");
        std::process::exit(0);
    }
    let raw_svg = if args.len() == 1 || args[1] == "-"{
	RawSVG::from_stdin()
	    .expect("Failed to read SVG from stdin!")
    } else {
	let svg_path = std::fs::canonicalize(&args[1])
	    .expect("Failed to interpret path as file!");
	RawSVG::from_file(&svg_path)
	    .expect("Failed to read SVG from file!")
    };
    // DISPLAY WINDOW
    let event_loop = EventLoop::<()>::with_user_event();
    let mut input = WinitInputHelper::new();
    let window = {
        WindowBuilder::new()
            .with_title("svgview")
            .with_resizable(true)
            .build(&event_loop)
            .unwrap()
    };


    // PIXEL BUFFER
    let mut pixels = {
	let window_size = window.inner_size();
        let surface_texture = SurfaceTexture::new(
	    window.inner_size().width,
	    window.inner_size().height,
	    &window);
        Pixels::new(window_size.width, window_size.height, surface_texture)?
    };
    pixels.set_clear_color(Color::WHITE);

    // APPLICATION STATE
    let evp = event_loop.create_proxy();
    let mut state = State::new(raw_svg, window.inner_size(), evp);

    // INTERFACE EVENT LOOP
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Draw the current frame
        if let Event::RedrawRequested(_) = event {
            // rasterize the SVG and copy the data to the pixel buffer
            let pixel_buffer = pixels.get_frame();
            pixel_buffer.copy_from_slice(state.pixels.data());

            if pixels
                .render()
                .map_err(|e| warn!("Rendering failed: {:?}", e))
                .is_err()
            {
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        if let Event::UserEvent(_) = event {
            state.handle_file_change();
            window.request_redraw();
        }

        // Handle input events
        if input.update(&event) {
            // Close events
            if input.key_pressed(VirtualKeyCode::Escape) || input.quit() {
                *control_flow = ControlFlow::Exit;
                return;
            }

            // Resize the window
            if let Some(size) = input.window_resized() {
                // resize pixel buffer, resize surface buffer, resize SVG buffer, then redraw
                pixels.resize_buffer(size.width, size.height);
                pixels.resize_surface(size.width, size.height);
                state.resize(size.width, size.height);
                window.request_redraw();
            }
        }
    });
}

struct RawSVG{
    original_path: Option<PathBuf>,
    document: usvg::Tree,
    opts: Options
}

impl RawSVG{
    pub fn from_file(file_path: &Path) -> Result<Self>{
	// let file_data = std::fs::read(&file).expect("Could not read input file!");
	let mut svg = std::fs::File::open(file_path)
	    .expect("Failed to open input file for reading!");

	let mut opts = usvg::Options {
            resources_dir: Some(file_path.to_path_buf()),
            ..Default::default()
        };
        opts.fontdb.load_system_fonts();
	let mut file_data = vec![];
	svg.read_to_end(&mut file_data)?;
	let document = usvg::Tree::from_data(&file_data, &opts.to_ref())?;
	Ok(Self{original_path: Some(file_path.to_path_buf()), document, opts})
    }
    pub fn from_stdin() -> Result<Self>{
	let mut opts = usvg::Options::default();
        opts.fontdb.load_system_fonts();
	let mut file_data = vec![];
	std::io::stdin().read_to_end(&mut file_data)?;
	let document = usvg::Tree::from_data(&file_data, &opts.to_ref())?;
	Ok(Self{original_path: None, document, opts})
    }
}

impl State {
    fn new(svg: RawSVG, window_size: PhysicalSize<u32>, evp: EventLoopProxy<()>) -> Self {
	// FILE WATCHER
	let watcher = svg.original_path.clone()
	    .map(|path|{
		let (tx, rx) = channel();
		let mut watcher = raw_watcher(tx)
		    .expect("Could not create filesystem watcher!");
		watcher
		    .watch(path, RecursiveMode::NonRecursive)
		    .expect("Could not start filesystem watcher!");

		thread::spawn(move || loop {
		    match rx.recv() {
			Ok(event) => {
			    if let Ok(Op::CLOSE_WRITE) = event.op {
				evp.send_event(())
				    .expect("Failed to notify UI of file write!");
			    }
			}
			Err(e) => warn!("watch error: {:?}", e),
		    }
		});

		watcher
	    });
        let mut state = Self {
	    _watcher: watcher,
            file: svg.original_path,
            width: window_size.width,
            height: window_size.height,

            options: svg.opts,
            pixels: Pixmap::new(window_size.width, window_size.height)
                .expect("Could not allocate memory for display!"),
            svg_data: svg.document,
        };
        state.rasterize_svg();
        state
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels =
            Pixmap::new(self.width, self.height).expect("Could not allocate memory for display!");
        self.rasterize_svg();
    }

    fn handle_file_change(&mut self) {
	if let Some(file) = &self.file{
            let svg_data = std::fs::read(&file).expect("Could not read input file!");
            self.svg_data = usvg::Tree::from_data(&svg_data, &self.options.to_ref())
		.expect("Could not parse data as SVG!");
            self.rasterize_svg();
	}
    }

    fn rasterize_svg(&mut self) {
        self.pixels
            .data_mut()
            .copy_from_slice(&vec![0; self.width as usize * self.height as usize * 4]);
        resvg::render(
            &self.svg_data,
            usvg::FitTo::Size(self.width, self.height),
            tiny_skia::Transform::default(),
            self.pixels.as_mut(),
        )
        .expect("Could not rasterize SVG!");
    }
}
