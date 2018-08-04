
use std::mem;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use sys_info;

use glium;

use window::Window;

use image_cache;
use image_cache::ImageCache;

#[derive(PartialEq)]
pub enum LoadRequest {
    None,
    LoadNext,
    LoadPrevious,
    LoadSpecific(PathBuf),
    Jump(i32),
}

#[derive(PartialEq, Copy, Clone)]
pub enum PlaybackState {
    Paused,
    Forward,
    //Backward,
}

pub struct PlaybackManager {
    playback_state: PlaybackState,

    image_cache: ImageCache,

    playback_start_time: Instant,
    frame_count_since_playback_start: u64,

    load_request: LoadRequest,

    should_sleep: bool,

    image_texture: Option<Rc<glium::texture::SrgbTexture2d>>,
}


impl PlaybackManager {
    pub fn new() -> Self {
        let cache_capaxity = match sys_info::mem_info() {
            Ok(value) => {
                // value originally reported in KiB
                ((value.total / 8) * 1024) as isize
            }
            _ => {
                println!("Could not get system memory size, using default value");
                // bytes
                500_000_000
            }
        };

        let thread_count = match sys_info::cpu_num() {
            Ok(value) => value.max(2).min(4),
            _ => 4,
        };

        let resulting_window = PlaybackManager {
            image_cache: ImageCache::new(cache_capaxity, thread_count),

            playback_state: PlaybackState::Paused,
            playback_start_time: Instant::now(),
            frame_count_since_playback_start: 0,
            load_request: LoadRequest::None,
            should_sleep: true,

            image_texture: None
        };

        resulting_window
    }

    pub fn playback_state(&self) -> PlaybackState {
        self.playback_state
    }

    pub fn start_playback_forward(&mut self) {
        self.playback_start_time = Instant::now();
        self.frame_count_since_playback_start = 0;
        self.playback_state = PlaybackState::Forward;
    }

    pub fn pause_playback(&mut self) {
        self.playback_state = PlaybackState::Paused;
    }

    pub fn current_filename<'a>(&'a self) -> &'a OsString {
        self.image_cache.current_filename()
    }

    pub fn current_file_path(&self) -> PathBuf {
        self.image_cache.current_file_path()
    }

    pub fn update_directory(&mut self) -> image_cache::Result<()> {
        self.image_cache.update_directory()
    }

    pub fn should_sleep(&self) -> bool {
        self.should_sleep
    }

    pub fn request_load(&mut self, request: LoadRequest) {
        self.load_request = request;
    }

    pub fn load_request<'a>(&'a self) -> &'a LoadRequest {
        &self.load_request
    }

    pub fn image_texture<'a>(&'a self) -> &'a Option<Rc<glium::texture::SrgbTexture2d>> {
        &self.image_texture
    }


    pub fn update_image(&mut self, window: &mut Window) {
        self.should_sleep = true;

        // The reason why I reset the load request in such a convoluted way is that
        // it has to guarantee that self.load_request will be reset even if I return from this
        // function early
        let mut load_request = LoadRequest::None;
        mem::swap(&mut self.load_request, &mut load_request);

        let framerate = 25.0;
        const NANOS_PER_SEC: u64 = 1000_000_000;
        let frame_delta_time_nanos = (NANOS_PER_SEC as f64 / framerate) as u64;

        if self.playback_state == PlaybackState::Paused {
            self.image_cache.process_prefetched(window.display()).unwrap();
            self.image_cache.send_load_requests();
        } else if load_request == LoadRequest::None {
            let elapsed = self.playback_start_time.elapsed();
            let elapsed_nanos =
                elapsed.as_secs() * NANOS_PER_SEC + elapsed.subsec_nanos() as u64;
            let frame_step =
                (elapsed_nanos / frame_delta_time_nanos) - self.frame_count_since_playback_start;
            if frame_step > 0 {
                load_request = match self.playback_state {
                    PlaybackState::Forward => LoadRequest::Jump(frame_step as i32),
                    //PlaybackState::Backward => LoadRequest::Jump(-(frame_step as i32)),
                    PlaybackState::Paused => unreachable!(),
                };
                self.frame_count_since_playback_start += frame_step;
            } else {
                self.image_cache.process_prefetched(window.display()).unwrap();

                let nanos_since_last = elapsed_nanos % frame_delta_time_nanos;
                const BUISY_WAIT_TRESHOLD: f32 = 0.8;
                if nanos_since_last
                    > (frame_delta_time_nanos as f32 * BUISY_WAIT_TRESHOLD) as u64
                {
                    // Just buisy wait if we are getting very close to the next frame swap
                    self.should_sleep = false;
                } else {
                    self.image_cache.send_load_requests();
                }
            }
        }

        //let should_sleep = load_request == LoadRequest::None && running && !update_screen;
        // Process long operations here
        let load_result = match load_request {
            LoadRequest::LoadNext => Some(self.image_cache.load_next(window.display())),
            LoadRequest::LoadPrevious => Some(self.image_cache.load_prev(window.display())),
            LoadRequest::LoadSpecific(ref file_path) => Some(
                if let Some(file_name) = file_path.file_name() {
                    self.image_cache
                        .load_specific(window.display(), file_path.as_ref())
                        .map(|x| (x, OsString::from(file_name)))
                } else {
                    Err(String::from("Could not extract filename").into())
                }
            ),
            LoadRequest::Jump(jump_count) => {
                Some(self.image_cache.load_jump(window.display(), jump_count))
            }
            LoadRequest::None => None,
        };
        if let Some(result) = load_result {
            match result {
                Ok((texture, filename)) => {
                    self.image_texture = Some(texture);
                    // FIXME the program hangs when the title is set during a resize
                    // this is due to the way glutin/winit is architected.
                    // An issu already exists in winit proposing to redesign
                    // the even loop.
                    // Until that is implemented the title is simply not updated during
                    // playback.
                    if self.playback_state == PlaybackState::Paused {
                        window.set_title_filename(filename.to_str().unwrap());
                    }
                }
                Err(err) => {
                    self.image_texture = None;
                    window.set_title_filename("[none]");
                    let stderr = &mut ::std::io::stderr();
                    let stderr_errmsg = "Error writing to stderr";
                    writeln!(stderr, "Error occured while loading image: {}", err)
                        .expect(stderr_errmsg);
                    for e in err.iter().skip(1) {
                        writeln!(stderr, "... caused by: {}", e).expect(stderr_errmsg);
                    }
                    if let Some(backtrace) = err.backtrace() {
                        writeln!(stderr, "backtrace: {:?}", backtrace).expect(stderr_errmsg);
                    }
                    writeln!(stderr).expect(stderr_errmsg);
                }
            }

            self.should_sleep = false;
        }
    }
}
