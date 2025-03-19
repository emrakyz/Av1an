// crates/av1an-core/src/context.rs
use std::{
    borrow::Cow,
    cmp,
    cmp::Reverse,
    convert::TryInto,
    ffi::OsString,
    fs,
    fs::File,
    io::{IsTerminal, Write},
    iter,
    path::{Path, PathBuf},
    process::{exit, Command, Stdio},
    sync::{
        atomic::{self, AtomicBool, AtomicUsize},
        mpsc,
        Arc,
    },
    thread,
};

use ansi_term::{Color, Style};
use anyhow::Context;
use av1_grain::TransferFunction;
use av1an_ffmpeg::{
    compose_ffmpeg_pipe,
    encode_audio,
    get_keyframes,
    num_frames,
};
use crossbeam_utils;
use itertools::Itertools;
use rand::{prelude::SliceRandom, thread_rng};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::ChildStderr,
};
use tracing::{debug, error, info, warn};

use crate::{
    broker::{Broker, EncoderCrash},
    create_dir,
    determine_workers,
    get_done,
    init_done,
    into_vec,
    progress_bar::{
        finish_progress_bar,
        inc_bar,
        inc_mp_bar,
        init_multi_progress_bar,
        init_progress_bar,
        reset_bar_at,
        reset_mp_bar_at,
        set_audio_size,
        update_mp_msg,
        update_mp_task,
        update_progress_bar_estimates,
    },
    read_task_queue,
    save_task_queue,
    scene_detect::av_scenechange_detect,
    scenes::Scene,
    settings::{EncodeArgs, InputPixelFormat},
    split::{extra_splits, segment, write_scenes_to_file},
    task::Task,
    vapoursynth::create_vs_file,
    DashMap,
    DoneJson,
    Input,
    SplitMethod,
    TaskMethod,
    TaskOrdering,
    Verbosity,
};

use av1an_output::{
    ffmpeg,
    ivf,
    mkvmerge,
    sort_files_by_filename,
    ConcatMethod,
};

#[derive(Debug)]
pub struct Av1anContext {
    pub frames:    usize,
    pub vs_script: Option<PathBuf>,
    pub args:      EncodeArgs,
}

impl Av1anContext {
    #[tracing::instrument]
    pub fn new(mut args: EncodeArgs) -> anyhow::Result<Self> {
        args.validate()?;
        let mut this = Self {
            frames: 0,
            vs_script: None,
            args,
        };
        this.initialize()?;
        Ok(this)
    }

    /// Initialize logging routines and create temporary directories
    #[tracing::instrument]
    fn initialize(&mut self) -> anyhow::Result<()> {
        ffmpeg::init()?;
        ffmpeg::util::log::set_level(ffmpeg::util::log::level::Level::Fatal);

        if !self.args.resume && Path::new(&self.args.temp).is_dir() {
            fs::remove_dir_all(&self.args.temp).with_context(|| {
                format!(
                    "Failed to remove temporary directory {:?}",
                    &self.args.temp
                )
            })?;
        }

        create_dir!(Path::new(&self.args.temp))?;
        create_dir!(Path::new(&self.args.temp).join("split"))?;
        create_dir!(Path::new(&self.args.temp).join("encode"))?;

        debug!("temporary directory: {}", &self.args.temp);

        let done_path = Path::new(&self.args.temp).join("done.json");
        let done_json_exists = done_path.exists();
        let tasks_json_exists = Path::new(&self.args.temp)
            .join("tasks.json")
            .exists();

        if self.args.resume {
            match (done_json_exists, tasks_json_exists) {
                // both files exist, so there is no problem
                (true, true) => {},
                (false, true) => {
                    info!(
                        "resume was set but done.json does not exist in \
                         temporary directory {:?}",
                        &self.args.temp
                    );
                    self.args.resume = false;
                },
                (true, false) => {
                    info!(
                        "resume was set but tasks.json does not exist in \
                         temporary directory {:?}",
                        &self.args.temp
                    );
                    self.args.resume = false;
                },
                (false, false) => {
                    info!(
                        "resume was set but neither tasks.json nor done.json \
                         exist in temporary directory {:?}",
                        &self.args.temp
                    );
                    self.args.resume = false;
                },
            }
        }

        if self.args.resume && done_json_exists {
            let done = fs::read_to_string(done_path)
                .with_context(|| "Failed to read contents of done.json")?;
            let done: DoneJson = serde_json::from_str(&done)
                .with_context(|| "Failed to parse done.json")?;
            self.frames = done.frames.load(atomic::Ordering::Relaxed);

            // frames need to be recalculated in this case
            if self.frames == 0 {
                self.frames = self.args.input.frames()?;
                done.frames
                    .store(self.frames, atomic::Ordering::Relaxed);
            }

            init_done(done);
        } else {
            init_done(DoneJson {
                frames:     AtomicUsize::new(0),
                done:       DashMap::new(),
                audio_done: AtomicBool::new(false),
            });

            let mut done_file = File::create(&done_path).unwrap();
            done_file
                .write_all(serde_json::to_string(get_done())?.as_bytes())?;
        };

        Ok(())
    }

    #[tracing::instrument]
    pub fn encode_file(&mut self) -> anyhow::Result<()> {
        let initial_frames = get_done()
            .done
            .iter()
            .map(|ref_multi| ref_multi.frames)
            .sum::<usize>();

        let vspipe_cache =
        // Technically we should check if the vapoursynth cache file exists rather than !self.resume,
        // but the code still works if we are resuming and the cache file doesn't exist (as it gets
        // generated when vspipe is first called), so it's not worth adding all the extra complexity.
        if (self.args.input.is_vapoursynth()
            || (self.args.input.is_video()
            && matches!(self.args.task_method, TaskMethod::LSMASH | TaskMethod::FFMS2 | TaskMethod::DGDECNV | TaskMethod::BESTSOURCE)))
            && !self.args.resume
        {
          self.vs_script = Some(match &self.args.input {
            Input::VapourSynth { path, .. } => path.clone(),
            Input::Video{ path } => create_vs_file(&self.args.temp, path, self.args.task_method)?,
          });

          let vs_script = self.vs_script.clone().unwrap();
          let vspipe_args = self.args.input.as_vspipe_args_vec()?;
          Some({
            thread::spawn(move || {
              let mut command = Command::new("vspipe");
              command.arg("-i")
                .arg(vs_script)
                .args(["-i", "-"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
              // Append vspipe arguments to the environment if there are any
              for arg in vspipe_args {
                command.args(["-a", &arg]);
              }
              command.status()
                .unwrap()
            })
          })
        } else {
          None
        };

        let res = self.args.input.resolution()?;
        let fps = self.args.input.frame_rate()?;
        let format = self.args.input.pixel_format()?;
        let tfc = self
            .args
            .input
            .transfer_function_params_adjusted(&self.args.video_params)?;
        info!(
            "Input: {}x{} @ {:.3} fps, {}, {}",
            res.0,
            res.1,
            fps,
            format,
            match tfc {
                TransferFunction::SMPTE2084 => "HDR",
                TransferFunction::BT1886 => "SDR",
            }
        );

        let splits = self.split_routine()?;

        if self.args.sc_only {
            debug!("scene detection only");

            if let Err(e) = fs::remove_dir_all(&self.args.temp) {
                warn!("Failed to delete temp directory: {}", e);
            }

            exit(0);
        }

        let (task_queue, total_tasks) = self.load_or_gen_task_queue(&splits)?;

        if self.args.resume {
            let tasks_done = get_done().done.len();
            info!(
                "encoding resumed with {}/{} tasks completed ({} remaining)",
                tasks_done,
                task_queue.len() + tasks_done,
                task_queue.len()
            );
        }

        if let Some(vspipe_cache) = vspipe_cache {
            vspipe_cache.join().unwrap();
        }

        crossbeam_utils::thread::scope(|s| -> anyhow::Result<()> {
            // vapoursynth audio is currently unsupported
            let audio_thread = if self.args.input.is_video()
                && (!self.args.resume
                    || !get_done()
                        .audio_done
                        .load(atomic::Ordering::SeqCst))
            {
                let input = self.args.input.as_video_path();
                let temp = self.args.temp.as_str();
                let audio_params = self.args.audio_params.as_slice();
                Some(s.spawn(move |_| {
                    let audio_output = encode_audio(input, temp, audio_params);
                    get_done()
                        .audio_done
                        .store(true, atomic::Ordering::SeqCst);

                    let progress_file = Path::new(temp).join("done.json");
                    let mut progress_file =
                        File::create(progress_file).unwrap();
                    progress_file
                        .write_all(
                            serde_json::to_string(get_done())
                                .unwrap()
                                .as_bytes(),
                        )
                        .unwrap();

                    if let Some(ref audio_output) = audio_output {
                        let audio_size = audio_output.metadata().unwrap().len();
                        set_audio_size(audio_size);
                    }

                    audio_output.is_some()
                }))
            } else {
                None
            };

            if self.args.workers == 0 {
                self.args.workers =
                    determine_workers(self.args.encoder) as usize;
            }
            self.args.workers = cmp::min(self.args.workers, task_queue.len());

            if std::io::stderr().is_terminal() {
                eprintln!(
                    "{}{} {} {}{} {} {}{} {}\n{}: {}",
                    Color::Green.bold().paint("Q"),
                    Color::Green.paint("ueue"),
                    Color::Green
                        .bold()
                        .paint(format!("{}", task_queue.len())),
                    Color::Blue.bold().paint("W"),
                    Color::Blue.paint("orkers"),
                    Color::Blue
                        .bold()
                        .paint(format!("{}", self.args.workers)),
                    Color::Purple.bold().paint("P"),
                    Color::Purple.paint("asses"),
                    Color::Purple
                        .bold()
                        .paint(format!("{}", self.args.passes)),
                    Style::default().bold().paint("Params"),
                    Style::default()
                        .dimmed()
                        .paint(self.args.video_params.join(" "))
                );
            } else {
                eprintln!(
                    "Queue {} Workers {} Passes {}\nParams: {}",
                    task_queue.len(),
                    self.args.workers,
                    self.args.passes,
                    self.args.video_params.join(" ")
                );
            }

            if self.args.verbosity == Verbosity::Normal {
                init_progress_bar(self.frames as u64, initial_frames as u64);
                reset_bar_at(initial_frames as u64);
            } else if self.args.verbosity == Verbosity::Verbose {
                init_multi_progress_bar(
                    self.frames as u64,
                    self.args.workers,
                    total_tasks,
                    initial_frames as u64,
                );
                reset_mp_bar_at(initial_frames as u64);
            }

            if !get_done().done.is_empty() {
                let frame_rate = self.args.input.frame_rate()?;
                update_progress_bar_estimates(
                    frame_rate,
                    self.frames,
                    self.args.verbosity,
                );
            }

            let broker = Broker {
                task_queue,
                project: self,
            };

            let (tx, rx) = mpsc::channel();
            let handle = s.spawn(|_| {
                broker.encoding_loop(tx, self.args.set_thread_affinity);
            });

            // Queue::encoding_loop only sends a message if there was an error
            // (meaning a task crashed) more than MAX_TRIES. So, we
            // have to explicitly exit the program if that happens.
            if rx.recv().is_ok() {
                exit(1);
            }

            handle.join().unwrap();

            finish_progress_bar();

            // TODO add explicit parameter to concatenation functions to control
            // whether audio is also muxed in
            let _audio_output_exists = audio_thread
                .map_or(false, |audio_thread| audio_thread.join().unwrap());

            debug!(
                "encoding finished, concatenating with {}",
                self.args.concat
            );

            match self.args.concat {
                ConcatMethod::Ivf => {
                    ivf(
                        &Path::new(&self.args.temp).join("encode"),
                        self.args.output_file.as_ref(),
                    )?;
                },
                ConcatMethod::MKVMerge => {
                    mkvmerge(
                        self.args.temp.as_ref(),
                        self.args.output_file.as_ref(),
                        self.args.encoder.into(),
                        total_tasks,
                    )?;
                },
                ConcatMethod::FFmpeg => {
                    ffmpeg(
                        self.args.temp.as_ref(),
                        self.args.output_file.as_ref(),
                    )?;
                },
            }

            if !Path::new(&self.args.output_file).exists() {
                warn!(
                    "Concatenation failed for unknown reasons! Temp folder \
                     will not be deleted: {}",
                    &self.args.temp
                );
            } else if !self.args.keep {
                if let Err(e) = fs::remove_dir_all(&self.args.temp) {
                    warn!("Failed to delete temp directory: {}", e);
                }
            }

            Ok(())
        })
        .unwrap()?;

        Ok(())
    }

    #[tracing::instrument]
    fn read_queue_files(source_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut queue_files = fs::read_dir(source_path)
            .with_context(|| {
                format!(
                    "Failed to read queue files from source path \
                     {source_path:?}"
                )
            })?
            .map(|res| res.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;

        queue_files.retain(|file| {
            file.is_file()
                && matches!(
                    file.extension().map(|ext| ext == "mkv"),
                    Some(true)
                )
        });
        sort_files_by_filename(&mut queue_files);

        Ok(queue_files)
    }

    /// Returns the number of frames encoded if crashed, to reset the progress
    /// bar.
    pub fn create_pipes(
        &self,
        task: &Task,
        current_pass: u8,
        worker_id: usize,
        padding: usize,
    ) -> Result<(), (Box<EncoderCrash>, u64)> {
        update_mp_task(worker_id, task.index, padding);

        let fpf_file = Path::new(&task.temp)
            .join("split")
            .join(format!("{}_fpf", task.name()));

        let video_params = task.video_params.clone();

        let enc_cmd = if task.passes == 1 {
            task.encoder.compose_1_1_pass(
                video_params,
                task.output(),
                task.frames(),
            )
        } else if current_pass == 1 {
            task.encoder.compose_1_2_pass(
                video_params,
                fpf_file.to_str().unwrap(),
                task.frames(),
            )
        } else {
            task.encoder.compose_2_2_pass(
                video_params,
                fpf_file.to_str().unwrap(),
                task.output(),
                task.frames(),
            )
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();

        let (
            source_pipe_stderr,
            ffmpeg_pipe_stderr,
            enc_output,
            enc_stderr,
            frame,
        ) = rt.block_on(async {
            let mut source_pipe = if let [source, args @ ..] = &*task.source_cmd
            {
                let mut command = tokio::process::Command::new(source);
                for arg in task.input.as_vspipe_args_vec().unwrap() {
                    command.args(["-a", &arg]);
                }
                command
                    .args(args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            } else {
                unreachable!()
            };

            let source_pipe_stdout: Stdio = source_pipe
                .stdout
                .take()
                .unwrap()
                .try_into()
                .unwrap();

            let source_pipe_stderr = source_pipe.stderr.take().unwrap();

            // converts the pixel format
            let create_ffmpeg_pipe =
                |pipe_from: Stdio, source_pipe_stderr: ChildStderr| {
                    let ffmpeg_pipe = compose_ffmpeg_pipe(
                        self.args.ffmpeg_filter_args.as_slice(),
                        self.args.output_pix_format.format,
                    );

                    let mut ffmpeg_pipe =
                        if let [ffmpeg, args @ ..] = &*ffmpeg_pipe {
                            tokio::process::Command::new(ffmpeg)
                                .args(args)
                                .stdin(pipe_from)
                                .stdout(Stdio::piped())
                                .stderr(Stdio::piped())
                                .spawn()
                                .unwrap()
                        } else {
                            unreachable!()
                        };

                    let ffmpeg_pipe_stdout: Stdio = ffmpeg_pipe
                        .stdout
                        .take()
                        .unwrap()
                        .try_into()
                        .unwrap();
                    let ffmpeg_pipe_stderr = ffmpeg_pipe.stderr.take().unwrap();
                    (
                        ffmpeg_pipe_stdout,
                        source_pipe_stderr,
                        Some(ffmpeg_pipe_stderr),
                    )
                };

            let (y4m_pipe, source_pipe_stderr, mut ffmpeg_pipe_stderr) =
                if self.args.ffmpeg_filter_args.is_empty() {
                    match &self.args.input_pix_format {
                        InputPixelFormat::FFmpeg {
                            format,
                        } => {
                            if self.args.output_pix_format.format == *format {
                                (source_pipe_stdout, source_pipe_stderr, None)
                            } else {
                                create_ffmpeg_pipe(
                                    source_pipe_stdout,
                                    source_pipe_stderr,
                                )
                            }
                        },
                        InputPixelFormat::VapourSynth {
                            bit_depth,
                        } => {
                            if self.args.output_pix_format.bit_depth
                                == *bit_depth
                            {
                                (source_pipe_stdout, source_pipe_stderr, None)
                            } else {
                                create_ffmpeg_pipe(
                                    source_pipe_stdout,
                                    source_pipe_stderr,
                                )
                            }
                        },
                    }
                } else {
                    create_ffmpeg_pipe(source_pipe_stdout, source_pipe_stderr)
                };

            let mut source_reader = BufReader::new(source_pipe_stderr).lines();
            let ffmpeg_reader = ffmpeg_pipe_stderr
                .take()
                .map(|stderr| BufReader::new(stderr).lines());

            let pipe_stderr =
                Arc::new(parking_lot::Mutex::new(String::with_capacity(128)));
            let p_stdr2 = Arc::clone(&pipe_stderr);

            let ffmpeg_stderr = if ffmpeg_reader.is_some() {
                Some(Arc::new(parking_lot::Mutex::new(String::with_capacity(
                    128,
                ))))
            } else {
                None
            };

            let f_stdr2 = ffmpeg_stderr.clone();

            tokio::spawn(async move {
                while let Some(line) = source_reader.next_line().await.unwrap()
                {
                    p_stdr2.lock().push_str(&line);
                    p_stdr2.lock().push('\n');
                }
            });
            if let Some(mut ffmpeg_reader) = ffmpeg_reader {
                let f_stdr2 = f_stdr2.unwrap();
                tokio::spawn(async move {
                    while let Some(line) =
                        ffmpeg_reader.next_line().await.unwrap()
                    {
                        f_stdr2.lock().push_str(&line);
                        f_stdr2.lock().push('\n');
                    }
                });
            }

            let mut enc_pipe = if let [encoder, args @ ..] = &*enc_cmd {
                tokio::process::Command::new(encoder)
                    .args(args)
                    .stdin(y4m_pipe)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            } else {
                unreachable!()
            };

            let mut frame = 0;

            let mut reader = BufReader::new(enc_pipe.stderr.take().unwrap());

            let mut buf = Vec::with_capacity(128);
            let mut enc_stderr = String::with_capacity(128);

            while let Ok(read) = reader.read_until(b'\r', &mut buf).await {
                if read == 0 {
                    break;
                }

                if let Ok(line) = simdutf8::basic::from_utf8_mut(&mut buf) {
                    if self.args.verbosity == Verbosity::Verbose
                        && !line.contains('\n')
                    {
                        update_mp_msg(worker_id, line.trim().to_string());
                    }
                    // This needs to be done before parse_encoded_frames, as it
                    // potentially mutates the string
                    enc_stderr.push_str(line);
                    enc_stderr.push('\n');

                    if current_pass == task.passes {
                        if let Some(new) =
                            task.encoder.parse_encoded_frames(line)
                        {
                            if new > frame {
                                if self.args.verbosity == Verbosity::Normal {
                                    inc_bar(new - frame);
                                } else if self.args.verbosity
                                    == Verbosity::Verbose
                                {
                                    inc_mp_bar(new - frame);
                                }
                                frame = new;
                            }
                        }
                    }
                }

                buf.clear();
            }

            let enc_output = enc_pipe.wait_with_output().await.unwrap();

            let source_pipe_stderr = pipe_stderr.lock().clone();
            let ffmpeg_pipe_stderr = ffmpeg_stderr.map(|x| x.lock().clone());
            (
                source_pipe_stderr,
                ffmpeg_pipe_stderr,
                enc_output,
                enc_stderr,
                frame,
            )
        });

        if !enc_output.status.success() {
            return Err((
                Box::new(EncoderCrash {
                    exit_status:        enc_output.status,
                    source_pipe_stderr: source_pipe_stderr.into(),
                    ffmpeg_pipe_stderr: ffmpeg_pipe_stderr.map(Into::into),
                    stderr:             enc_stderr.into(),
                    stdout:             enc_output.stdout.into(),
                }),
                frame,
            ));
        }

        if current_pass == task.passes {
            let encoded_frames = num_frames(task.output().as_ref());

            let err_str = match encoded_frames {
                Ok(encoded_frames)
                    if !task.ignore_frame_mismatch
                        && encoded_frames != task.frames() =>
                {
                    Some(format!(
                        "FRAME MISMATCH: task {}: {encoded_frames}/{} \
                         (actual/expected frames)",
                        task.index,
                        task.frames()
                    ))
                },
                Err(error) => Some(format!(
                    "FAILED TO COUNT FRAMES: task {}: {error}",
                    task.index
                )),
                _ => None,
            };

            if let Some(err_str) = err_str {
                return Err((
                    Box::new(EncoderCrash {
                        exit_status:        enc_output.status,
                        source_pipe_stderr: source_pipe_stderr.into(),
                        ffmpeg_pipe_stderr: ffmpeg_pipe_stderr.map(Into::into),
                        stderr:             enc_stderr.into(),
                        stdout:             err_str.into(),
                    }),
                    frame,
                ));
            }
        }

        Ok(())
    }

    fn create_encoding_queue(
        &self,
        scenes: &[Scene],
    ) -> anyhow::Result<Vec<Task>> {
        let mut tasks = match &self.args.input {
            Input::Video {
                ..
            } => match self.args.task_method {
                TaskMethod::FFMS2
                | TaskMethod::LSMASH
                | TaskMethod::DGDECNV
                | TaskMethod::BESTSOURCE => {
                    let vs_script = self.vs_script.as_ref().unwrap().as_path();
                    self.create_video_queue_vs(scenes, vs_script)
                },
                TaskMethod::Hybrid => self.create_video_queue_hybrid(scenes)?,
                TaskMethod::Select => self.create_video_queue_select(scenes),
                TaskMethod::Segment => {
                    self.create_video_queue_segment(scenes)?
                },
            },
            Input::VapourSynth {
                path, ..
            } => self.create_video_queue_vs(scenes, path.as_path()),
        };

        match self.args.task_order {
            TaskOrdering::LongestFirst => {
                tasks.sort_unstable_by_key(|task| Reverse(task.frames()));
            },
            TaskOrdering::ShortestFirst => {
                tasks.sort_unstable_by_key(Task::frames);
            },
            TaskOrdering::Sequential => {},
            TaskOrdering::Random => {
                tasks.shuffle(&mut thread_rng());
            },
        }

        Ok(tasks)
    }

    fn calc_split_locations(&self) -> anyhow::Result<(Vec<Scene>, usize)> {
        Ok(match self.args.split_method {
            SplitMethod::AvScenechange => av_scenechange_detect(
                &self.args.input,
                self.args.encoder,
                self.frames,
                self.args.min_scene_len,
                self.args.verbosity,
                self.args.sc_pix_format,
                self.args.sc_method,
                self.args.sc_downscale_height,
            )?,
            SplitMethod::None => {
                let frames = self.args.input.frames()?;
                (
                    vec![Scene {
                        start_frame: 0, end_frame: frames
                    }],
                    frames,
                )
            },
        })
    }

    // If we are not resuming, then do scene detection. Otherwise: get scenes
    // from scenes.json and return that.
    fn split_routine(&mut self) -> anyhow::Result<Vec<Scene>> {
        let scene_file = self.args.scenes.as_ref().map_or_else(
            || Cow::Owned(Path::new(&self.args.temp).join("scenes.json")),
            |path| Cow::Borrowed(path.as_path()),
        );

        let used_existing_cuts;
        let (mut scenes, frames) = if (self.args.scenes.is_some()
            && scene_file.exists())
            || self.args.resume
        {
            used_existing_cuts = true;
            crate::split::read_scenes_from_file(scene_file.as_ref())?
        } else {
            used_existing_cuts = false;
            self.frames = self.args.input.frames()?;
            self.calc_split_locations()?
        };
        self.frames = frames;
        get_done()
            .frames
            .store(self.frames, atomic::Ordering::SeqCst);

        // Add forced keyframes
        for kf in &self.args.force_keyframes {
            if let Some((scene_pos, s)) = scenes
                .iter_mut()
                .find_position(|s| (s.start_frame..s.end_frame).contains(kf))
            {
                if *kf == s.start_frame {
                    // Already a keyframe
                    continue;
                }
                // Split this scene into two scenes at the requested keyframe
                let mut new = s.clone();
                s.end_frame = *kf;
                new.start_frame = *kf;
                scenes.insert(scene_pos + 1, new);
            } else {
                warn!(
                    "scene {} was requested as a forced keyframe but video \
                     has {} frames, ignoring",
                    *kf, frames
                );
            }
        }

        let scenes_before = scenes.len();
        if !used_existing_cuts {
            if let Some(split_len @ 1..) = self.args.extra_splits_len {
                scenes = extra_splits(&scenes, self.frames, split_len);
                let scenes_after = scenes.len();
                info!(
                    "scenecut: found {} scene(s) [with extra_splits ({} \
                     frames): {} scene(s)]",
                    scenes_before, split_len, scenes_after
                );
            } else {
                info!("scenecut: found {} scene(s)", scenes_before);
            }
        }

        write_scenes_to_file(&scenes, self.frames, scene_file)?;

        Ok(scenes)
    }

    fn create_select_task(
        &self,
        index: usize,
        src_path: &Path,
        start_frame: usize,
        end_frame: usize,
        frame_rate: f64,
    ) -> anyhow::Result<Task> {
        assert!(start_frame < end_frame, "Can't make a task with <= 0 frames!");

        let ffmpeg_gen_cmd: Vec<OsString> = into_vec![
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            src_path,
            "-vf",
            format!("select=between(n\\,{}\\,{})", start_frame, end_frame - 1),
            "-pix_fmt",
            self.args
                .output_pix_format
                .format
                .descriptor()
                .unwrap()
                .name(),
            "-strict",
            "-1",
            "-f",
            "yuv4mpegpipe",
            "-",
        ];

        let output_ext = self.args.encoder.output_extension();

        let task = Task {
            temp: self.args.temp.clone(),
            index,
            input: Input::Video {
                path: src_path.to_path_buf()
            },
            source_cmd: ffmpeg_gen_cmd,
            output_ext: output_ext.to_owned(),
            start_frame,
            end_frame,
            frame_rate,
            video_params: self.args.video_params.clone(),
            passes: self.args.passes,
            encoder: self.args.encoder,
            ignore_frame_mismatch: self.args.ignore_frame_mismatch,
        };
        Ok(task)
    }

    fn create_vs_task(
        &self,
        index: usize,
        vs_script: &Path,
        scene: &Scene,
        frame_rate: f64,
    ) -> anyhow::Result<Task> {
        // the frame end boundary is actually a frame that should be included in
        // the next task
        let frame_end = scene.end_frame - 1;

        let vspipe_cmd_gen: Vec<OsString> = into_vec![
            "vspipe",
            vs_script,
            "-c",
            "y4m",
            "-",
            "-s",
            scene.start_frame.to_string(),
            "-e",
            frame_end.to_string(),
        ];

        let output_ext = self.args.encoder.output_extension();

        let task = Task {
            temp: self.args.temp.clone(),
            index,
            input: Input::VapourSynth {
                path:        vs_script.to_path_buf(),
                vspipe_args: self.args.input.as_vspipe_args_vec()?,
            },
            source_cmd: vspipe_cmd_gen,
            output_ext: output_ext.to_owned(),
            start_frame: scene.start_frame,
            end_frame: scene.end_frame,
            frame_rate,
            video_params: self.args.video_params.clone(),
            passes: self.args.passes,
            encoder: self.args.encoder,
            ignore_frame_mismatch: self.args.ignore_frame_mismatch,
        };
        Ok(task)
    }

    fn create_video_queue_vs(
        &self,
        scenes: &[Scene],
        vs_script: &Path,
    ) -> Vec<Task> {
        let frame_rate = self.args.input.frame_rate().unwrap();
        let task_queue: Vec<Task> = scenes
            .iter()
            .enumerate()
            .map(|(index, scene)| {
                self.create_vs_task(index, vs_script, scene, frame_rate)
                    .unwrap()
            })
            .collect();

        task_queue
    }

    fn create_video_queue_select(&self, scenes: &[Scene]) -> Vec<Task> {
        let input = self.args.input.as_video_path();
        let frame_rate = self.args.input.frame_rate().unwrap();

        let task_queue: Vec<Task> = scenes
            .iter()
            .enumerate()
            .map(|(index, scene)| {
                self.create_select_task(
                    index,
                    input,
                    scene.start_frame,
                    scene.end_frame,
                    frame_rate,
                )
                .unwrap()
            })
            .collect();

        task_queue
    }

    fn create_video_queue_segment(
        &self,
        scenes: &[Scene],
    ) -> anyhow::Result<Vec<Task>> {
        let input = self.args.input.as_video_path();
        let frame_rate = self.args.input.frame_rate().unwrap();

        debug!("Splitting video");
        segment(
            input,
            &self.args.temp,
            &scenes
                .iter()
                .skip(1)
                .map(|scene| scene.start_frame)
                .collect::<Vec<usize>>(),
        );
        debug!("Splitting done");

        let source_path = Path::new(&self.args.temp).join("split");
        let queue_files = Self::read_queue_files(&source_path)?;

        assert!(
            !queue_files.is_empty(),
            "Error: No files found in temp/split, probably splitting not \
             working"
        );

        let task_queue: Vec<Task> = queue_files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                self.create_task_from_segment(
                    index,
                    file.as_path().to_str().unwrap(),
                    frame_rate,
                )
                .unwrap()
            })
            .collect();

        Ok(task_queue)
    }

    fn create_video_queue_hybrid(
        &self,
        scenes: &[Scene],
    ) -> anyhow::Result<Vec<Task>> {
        let input = self.args.input.as_video_path();
        let frame_rate = self.args.input.frame_rate().unwrap();

        let keyframes = get_keyframes(input).unwrap();

        let to_split: Vec<usize> = keyframes
            .iter()
            .filter(|kf| {
                scenes
                    .iter()
                    .any(|scene| scene.start_frame == **kf)
            })
            .copied()
            .collect();

        debug!("Segmenting video");
        segment(input, &self.args.temp, &to_split[1..]);
        debug!("Segment done");

        let source_path = Path::new(&self.args.temp).join("split");
        let queue_files = Self::read_queue_files(&source_path)?;

        let kf_list = to_split
            .iter()
            .copied()
            .chain(iter::once(self.frames))
            .tuple_windows();

        let mut segments = Vec::with_capacity(scenes.len());
        for (file, (x, y)) in queue_files.iter().zip(kf_list) {
            for s in scenes {
                let s0 = s.start_frame;
                let s1 = s.end_frame;
                if s0 >= x && s1 <= y && s0 < s1 {
                    segments.push((file.as_path(), (s0 - x, s1 - x, s)));
                }
            }
        }

        let task_queue: Vec<Task> = segments
            .iter()
            .enumerate()
            .map(|(index, &(file, (start, end, scene)))| {
                self.create_select_task(index, file, start, end, frame_rate)
                    .unwrap()
            })
            .collect();

        Ok(task_queue)
    }

    #[tracing::instrument]
    fn create_task_from_segment(
        &self,
        index: usize,
        file: &str,
        frame_rate: f64,
    ) -> anyhow::Result<Task> {
        let ffmpeg_gen_cmd: Vec<OsString> = into_vec![
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            file.to_owned(),
            "-strict",
            "-1",
            "-pix_fmt",
            self.args
                .output_pix_format
                .format
                .descriptor()
                .unwrap()
                .name(),
            "-f",
            "yuv4mpegpipe",
            "-",
        ];

        let output_ext = self.args.encoder.output_extension();

        let num_frames = num_frames(Path::new(file))?;

        let task = Task {
            temp: self.args.temp.clone(),
            input: Input::Video {
                path: PathBuf::from(file)
            },
            source_cmd: ffmpeg_gen_cmd,
            output_ext: output_ext.to_owned(),
            index,
            start_frame: 0,
            end_frame: num_frames,
            frame_rate,
            video_params: self.args.video_params.clone(),
            passes: self.args.passes,
            encoder: self.args.encoder,
            ignore_frame_mismatch: self.args.ignore_frame_mismatch,
        };
        Ok(task)
    }

    /// Returns unfinished tasks and number of total tasks
    fn load_or_gen_task_queue(
        &self,
        splits: &[Scene],
    ) -> anyhow::Result<(Vec<Task>, usize)> {
        if self.args.resume {
            let mut tasks = read_task_queue(self.args.temp.as_ref())?;
            let num_tasks = tasks.len();

            let done = get_done();

            // only keep the tasks that are not done
            tasks.retain(|task| !done.done.contains_key(&task.name()));

            Ok((tasks, num_tasks))
        } else {
            let tasks = self.create_encoding_queue(splits)?;
            let num_tasks = tasks.len();
            save_task_queue(&self.args.temp, &tasks)?;
            Ok((tasks, num_tasks))
        }
    }
}
