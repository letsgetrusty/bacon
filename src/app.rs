use {
    crate::*,
    anyhow::Result,
    crokey::*,
    std::{
        io::Write,
        time::Duration,
    },
    termimad::{
        EventSource,
        EventSourceOptions,
        Ticker,
        crossbeam::channel::select,
        crossterm::event::Event,
    },
};

#[cfg(windows)]
use {
    crokey::key,
    termimad::crossterm::event::{
        MouseEvent,
        MouseEventKind,
    },
};

enum DoAfterMission {
    NextJob(JobRef),
    ReloadConfig,
    Quit,
}

impl From<JobRef> for DoAfterMission {
    fn from(job_ref: JobRef) -> Self {
        Self::NextJob(job_ref)
    }
}

/// Run the application until the user quits
pub fn run(
    w: &mut W,
    mut settings: Settings,
    args: &Args,
    location: Context,
    headless: bool,
) -> Result<()> {
    let event_source = if headless {
        // in headless mode, in some contexts, ctrl-c might not be enough to kill
        // bacon so we add this handler
        ctrlc::set_handler(move || {
            eprintln!("bye");
            std::process::exit(0);
        })
        .expect("Error setting Ctrl-C handler");
        None
    } else {
        Some(EventSource::with_options(EventSourceOptions {
            combine_keys: false,
            ..Default::default()
        })?)
    };
    let mut job_stack = JobStack::default();
    let mut next_job = JobRef::Initial;
    let mut message = None;
    loop {
        let (concrete_job_ref, job) = match job_stack.pick_job(&next_job, &settings)? {
            Some(t) => t,
            None => {
                break;
            }
        };
        let mission = location.mission(concrete_job_ref, job, &settings)?;
        let do_after =
            app::run_mission(w, mission, event_source.as_ref(), message.take(), headless)?;
        match do_after {
            DoAfterMission::NextJob(job_ref) => {
                next_job = job_ref;
            }
            DoAfterMission::ReloadConfig => match Settings::read(args, &location) {
                Ok(new_settings) => {
                    settings = new_settings;
                    message = Some(Message::short("Config reloaded"));
                }
                Err(e) => {
                    message = Some(Message::short(format!("Invalid config: {e}")));
                }
            },
            DoAfterMission::Quit => {
                break;
            }
        }
    }
    Ok(())
}

/// Run the mission and return what to do afterwards
fn run_mission(
    w: &mut W,
    mission: Mission,
    event_source: Option<&EventSource>,
    message: Option<Message>,
    headless: bool,
) -> Result<DoAfterMission> {
    let keybindings = mission.settings.keybindings.clone();

    // build the watcher detecting and transmitting mission file changes
    let ignorer = time!(Info, mission.ignorer());
    let mission_watcher = Watcher::new(&mission.paths_to_watch, ignorer)?;

    // create the watcher for config file changes
    let config_watcher = Watcher::new(&mission.settings.config_files, IgnorerSet::default())?;

    // create the executor, mission, and state
    let mut executor = MissionExecutor::new(&mission)?;
    let on_change_strategy = mission
        .job
        .on_change_strategy
        .or(mission.settings.on_change_strategy)
        .unwrap_or(OnChangeStrategy::WaitThenRestart);
    let mut state = AppState::new(mission, headless)?;
    if let Some(message) = message {
        state.messages.push(message);
    }
    state.computation_starts();
    if !headless {
        state.draw(w)?;
    }
    let mut task_executor = executor.start(state.new_task())?; // first computation

    // A very low frequency tick generator, to ensure "config loaded" message doesn't stick
    // too long on the screen
    let mut ticker = Ticker::new();
    ticker.tick_infinitely((), Duration::from_secs(5));

    let _dummy_sender;
    let user_events = if let Some(event_source) = event_source {
        event_source.receiver()
    } else {
        let (sender, receiver) = termimad::crossbeam::channel::unbounded();
        _dummy_sender = sender;
        receiver
    };
    let mut do_after_mission = DoAfterMission::Quit;

    // loop on events
    #[allow(unused_mut)]
    loop {
        let mut action: Option<&Action> = None;
        select! {
            recv(ticker.tick_receiver) -> _ => {
                // just redraw
            }
            recv(mission_watcher.receiver) -> _ => {
                debug!("watch event received");
                if task_executor.is_in_grace_period() {
                    debug!("ignoring notify event in grace period");
                    continue;
                }
                state.receive_watch_event();
                if state.auto_refresh.is_enabled() {
                    if !state.is_computing() || on_change_strategy == OnChangeStrategy::KillThenRestart {
                        action = Some(&Action::Internal(Internal::ReRun));
                    }
                }
            }
            recv(config_watcher.receiver) -> _ => {
                info!("config watch event received");
                action = Some(&Action::Internal(Internal::ReloadConfig));
            }
            recv(executor.line_receiver) -> info => {
                if let Ok(info) = info {
                    match info {
                        CommandExecInfo::Line(line) => {
                            if headless {
                                match line.origin {
                                    CommandStream::StdOut => print!("{}", line.content),
                                    CommandStream::StdErr => eprint!("{}", line.content),
                                }
                            }
                            let line = line.into();
                            state.add_line(line);
                        }
                        CommandExecInfo::End { status } => {
                            // computation finished
                            info!("execution finished with status: {:?}", status);
                            state.finish_task(status)?;
                            if headless {
                                for badge in state.job_badges() {
                                    badge.draw(w)?;
                                }
                                write!(w, "\n")?;
                                w.flush()?;
                            }
                            action = state.action();
                        }
                        CommandExecInfo::Error(e) => {
                            state.computation_stops();
                            return Err(e.context(format!("error in computation for job '{}'", state.mission.concrete_job_ref.badge_label())));
                        }
                        CommandExecInfo::Interruption => {
                            debug!("command was interrupted (by us)");
                        }
                    }
                }
            }
            recv(user_events) -> user_event => {
                match user_event?.event {
                    Event::Resize(mut width, mut height) => {
                        state.resize(width, height);
                    }
                    Event::Key(key_event) => {
                        let key_combination = KeyCombination::from(key_event);
                        debug!("key combination pressed: {}", key_combination);
                        if !state.apply_key_combination(key_combination) {
                            action = keybindings.get(key_combination);
                        }
                    }
                    #[cfg(windows)]
                    Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, .. }) => {
                        action = keybindings.get(key!(down));
                    }
                    #[cfg(windows)]
                    Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp, .. }) => {
                        action = keybindings.get(key!(up));
                    }
                    _ => {}
                }
                if let Some(event_source) = event_source {
                    event_source.unblock(false);
                }
            }
        }
        if let Some(action) = action.take() {
            debug!("requested action: {action:?}");
            match action {
                Action::Export(export_name) => {
                    let export_name = export_name.to_string();
                    state
                        .mission
                        .settings
                        .exports
                        .do_named_export(&export_name, &state);
                    state
                        .messages
                        .push(Message::short(format!("Export *{}* done", &export_name)));
                }
                Action::Internal(internal) => match internal {
                    Internal::Back => {
                        if !state.back() {
                            do_after_mission = DoAfterMission::NextJob(JobRef::Previous);
                            break;
                        }
                    }
                    Internal::NextMatch => {
                        state.next_match();
                    }
                    Internal::PreviousMatch => {
                        state.previous_match();
                    }
                    Internal::FocusSearch => {
                        state.focus_search();
                    }
                    Internal::Help => {
                        state.toggle_help();
                    }
                    Internal::Pause => {
                        state.auto_refresh = AutoRefresh::Paused;
                    }
                    Internal::Quit => {
                        break;
                    }
                    Internal::ReRun => {
                        task_executor.die();
                        task_executor = state.start_computation(&mut executor)?;
                    }
                    Internal::Refresh => {
                        state.clear();
                        task_executor.die();
                        task_executor = state.start_computation(&mut executor)?;
                    }
                    Internal::ReloadConfig => {
                        do_after_mission = DoAfterMission::ReloadConfig;
                        break;
                    }
                    Internal::ScopeToFailures => {
                        if let Some(scope) = state.failures_scope() {
                            info!("scoping to failures: {scope:#?}");
                            do_after_mission = JobRef::from(scope).into();
                            break;
                        } else {
                            warn!("no available failures scope");
                        }
                    }
                    Internal::Scroll(scroll_command) => {
                        let scroll_command = *scroll_command;
                        state.apply_scroll_command(scroll_command);
                    }
                    Internal::ToggleBacktrace(level) => {
                        state.toggle_backtrace(level);
                        task_executor.die();
                        task_executor = state.start_computation(&mut executor)?;
                    }
                    Internal::TogglePause => match state.auto_refresh {
                        AutoRefresh::Enabled => {
                            state.auto_refresh = AutoRefresh::Paused;
                        }
                        AutoRefresh::Paused => {
                            if state.changes_since_last_job_start > 0 {
                                state.clear();
                                task_executor.die();
                                task_executor = state.start_computation(&mut executor)?;
                            }
                            state.auto_refresh = AutoRefresh::Enabled;
                        }
                    },
                    Internal::ToggleRawOutput => {
                        state.toggle_raw_output();
                    }
                    Internal::ToggleSummary => {
                        state.toggle_summary_mode();
                    }
                    Internal::ToggleWrap => {
                        state.toggle_wrap_mode();
                    }
                    Internal::Unpause => {
                        if state.changes_since_last_job_start > 0 {
                            state.clear();
                            task_executor.die();
                            task_executor = state.start_computation(&mut executor)?;
                        }
                        state.auto_refresh = AutoRefresh::Enabled;
                    }
                    Internal::Validate => {
                        state.validate();
                    }
                },
                Action::Job(job_ref) => {
                    do_after_mission = job_ref.clone().into();
                    break;
                }
            }
        }
        if !headless {
            state.draw(w)?;
        }
    }
    task_executor.die();
    Ok(do_after_mission)
}
