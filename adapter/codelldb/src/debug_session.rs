mod breakpoints;
mod debugger_terminal;
mod disassemble;
mod launch;
mod step_in;
mod variables;

use crate::debug_event_listener::DebugEventListener;
use crate::prelude::*;

use crate::cancellation;
use crate::dap_session::DAPSession;
use crate::disassembly;
use crate::expressions;
use crate::fsutil::normalize_path;
use crate::handles::{self, HandleTree};
use crate::must_initialize::{Initialized, MustInitialize, NotInitialized};
use crate::platform::{get_fs_path_case, pipe};
use crate::python::PythonEvent;
use crate::python::{PythonInterface, PythonSession};
use crate::shared::Shared;
use crate::terminal::Terminal;
use breakpoints::Breakpoints;
use debugger_terminal::DebuggerTerminal;
use variables::Container;

use std;
use std::cell::RefCell;
use std::cmp;
use std::collections::HashMap;
use std::env;
use std::ffi::CStr;
use std::fmt::Write;
use std::io::{Cursor, LineWriter};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str;
use std::sync::Arc;
use std::time;

use adapter_protocol::*;
use futures;
use futures::prelude::*;
use lldb::*;
use serde_json;
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc};

pub struct DebugSession {
    self_ref: MustInitialize<Shared<DebugSession>>,
    dap_session: DAPSession,
    python: Option<PythonSession>,
    debug_event_listener: Arc<DebugEventListener>,
    current_cancellation: cancellation::Receiver, // Cancellation associated with the current request
    configuration_done_sender: broadcast::Sender<()>,
    console_pipe: std::fs::File,

    debugger: SBDebugger,
    debugger_terminal: Option<DebuggerTerminal>,
    target: SBTarget,
    terminate_on_disconnect: bool,
    no_debug: bool,

    breakpoints: RefCell<Breakpoints>,
    var_refs: HandleTree<Container>,
    disasm_ranges: disassembly::DisassembledRanges,
    source_map_cache: RefCell<HashMap<PathBuf, Option<Rc<PathBuf>>>>,
    relative_path_base: MustInitialize<PathBuf>,
    pre_terminate_commands: Option<Vec<String>>,
    exit_commands: Option<Vec<String>>,
    debuggee_terminal: Option<Terminal>,
    selected_frame_changed: bool,
    last_goto_request: Option<GotoTargetsArguments>,
    step_in_targets: Vec<step_in::StepInTargetInternal>,

    client_caps: MustInitialize<InitializeRequestArguments>,

    default_expr_type: Expressions,
    global_format: Format,
    show_disassembly: ShowDisassembly,
    deref_pointers: bool,
    console_mode: ConsoleMode,
    suppress_missing_files: bool,
    evaluate_for_hovers: bool,
    command_completions: bool,
    evaluation_timeout: time::Duration,
    source_languages: Vec<String>,
    breakpoint_mode: BreakpointMode,
    summary_timeout: time::Duration,
    max_summary_length: usize,
    max_instr_bytes: usize,
}

// AsyncResponse is used to "smuggle" futures out of request handlers
// in the few cases when we need to respond asynchronously.
struct AsyncResponse(pub Box<dyn Future<Output = Result<ResponseBody, Error>> + 'static>);

impl std::error::Error for AsyncResponse {}
impl std::fmt::Debug for AsyncResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "AsyncResponse")
    }
}
impl std::fmt::Display for AsyncResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "AsyncResponse")
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////

unsafe impl Send for DebugSession {}

impl DebugSession {
    pub fn run(
        dap_session: DAPSession,
        settings: AdapterSettings,
        python_interface: Option<Arc<PythonInterface>>,
    ) -> impl Future {
        let debugger = SBDebugger::create(false);
        debugger.set_async_mode(true);

        let (con_reader, con_writer) = pipe().unwrap();

        let (python, mut python_events_stream) = if let Some(python_interface) = python_interface {
            let (python, python_events) = python_interface.new_session(&debugger, con_writer.try_clone().unwrap());
            (Some(python), python_events)
        } else {
            let (_, receiver) = mpsc::channel(1);
            (None, receiver)
        };

        let target = debugger.dummy_target();

        let mut debug_session = DebugSession {
            self_ref: NotInitialized,
            dap_session: dap_session,
            python: python,
            debug_event_listener: DebugEventListener::new(),
            current_cancellation: cancellation::dummy(),
            configuration_done_sender: broadcast::channel(1).0,
            console_pipe: con_writer,

            debugger: debugger,
            target: target.clone(),
            debugger_terminal: None,
            terminate_on_disconnect: false,
            no_debug: false,

            breakpoints: RefCell::new(Breakpoints::new()),
            var_refs: HandleTree::new(),
            disasm_ranges: disassembly::DisassembledRanges::new(&target),
            source_map_cache: RefCell::new(HashMap::new()),
            relative_path_base: NotInitialized,
            pre_terminate_commands: None,
            exit_commands: None,
            debuggee_terminal: None,
            selected_frame_changed: false,
            last_goto_request: None,
            step_in_targets: Vec::new(),

            client_caps: NotInitialized,

            default_expr_type: Expressions::Simple,
            global_format: Format::Default,
            show_disassembly: ShowDisassembly::Auto,
            deref_pointers: true,
            console_mode: ConsoleMode::Commands,
            suppress_missing_files: true,
            evaluate_for_hovers: true,
            command_completions: true,
            evaluation_timeout: time::Duration::from_secs(5),
            source_languages: vec!["cpp".into()],
            breakpoint_mode: BreakpointMode::Path,
            summary_timeout: time::Duration::from_millis(10),
            max_summary_length: 32,
            max_instr_bytes: 9,
        };

        let con_reader = tokio::fs::File::from_std(con_reader);
        DebugSession::pipe_console_ouput(&debug_session.dap_session, con_reader);

        debug_session.update_adapter_settings(&settings);

        let mut requests_receiver = DebugSession::cancellation_filter(&debug_session.dap_session.clone());
        let mut debug_events_stream =
            debug_session.debug_event_listener.start_polling(&debug_session.debugger.listener(), 1000);

        let con_writer = debug_session.console_pipe.try_clone().unwrap();
        log_errors!(debug_session.debugger.set_output_file(SBFile::from(con_writer, false)));

        let shared_session = Shared::new(debug_session);
        shared_session.try_map(|s| s.self_ref = Initialized(shared_session.clone())).unwrap();

        // The main event loop, where we react to incoming events from different sources.
        let local_set = tokio::task::LocalSet::new();
        local_set.spawn_local(async move {
            loop {
                tokio::select! {
                    // Python events
                    Some(event) = python_events_stream.recv() => {
                        shared_session.map(|s| s.handle_python_event(event)).await;
                    }
                    // LLDB events.
                    Some(event) = debug_events_stream.recv() => {
                        shared_session.map(|s| s.handle_debug_event(event)).await;
                    }
                    // Requests from VSCode
                    request = requests_receiver.recv() => {
                        match request {
                            Some((seq, request, cancellation)) => shared_session.map(
                                    |s| s.handle_request(seq, request, cancellation)).await,
                            None => {
                                debug!("End of the requests stream");
                                break;
                            }
                        }
                    }
                }
            }

            // Session shutdown.
            shared_session
                .map(|s| {
                    log_errors!(s.destroy_debugger_terminal());
                    s.self_ref = NotInitialized;
                })
                .await;

            SBBreakpoint::clear_all_callbacks(); // Callbacks hold references to the session.

            // There shouldn't be any other references at this point.
            if shared_session.ref_count() > 1 {
                error!("shared_session.ref_count={}", shared_session.ref_count());
            }
        });

        local_set
    }

    // Pipe data received from con_reader to the DAP session as "console" events.
    fn pipe_console_ouput(dap_session: &DAPSession, mut con_reader: tokio::fs::File) {
        use std::io::Write;
        let dap_session = dap_session.clone();
        tokio::spawn(async move {
            let mut con_data = [0u8; 1024];
            let mut line_buf = LineWriter::new(Cursor::new(Vec::<u8>::new()));
            loop {
                if let Ok(count) = con_reader.read(&mut con_data).await {
                    if count == 0 {
                        debug!("End of the console stream");
                        break;
                    }
                    log_errors!(line_buf.write(&con_data[..count]));

                    let writer_pos = line_buf.get_ref().position() as usize;
                    if writer_pos > 0 {
                        let output = &line_buf.get_ref().get_ref()[..writer_pos];
                        let output = String::from_utf8_lossy(output).into_owned();
                        let event = EventBody::output(OutputEventBody {
                            output: output,
                            category: Some("console".into()),
                            ..Default::default()
                        });
                        log_errors!(dap_session.send_event(event).await);
                        line_buf.get_mut().set_position(0);
                    }
                }
            }
        });
    }

    /// Handle request cancellations.
    fn cancellation_filter(
        dap_session: &DAPSession,
    ) -> mpsc::Receiver<(u32, RequestArguments, cancellation::Receiver)> {
        use broadcast::error::RecvError;

        let mut raw_requests_stream = dap_session.subscribe_requests().unwrap();
        let (requests_sender, requests_receiver) =
            mpsc::channel::<(u32, RequestArguments, cancellation::Receiver)>(100);

        // This task pairs incoming requests with a cancellation token, which is activated upon receiving a "cancel" request.
        let filter = async move {
            let mut pending_requests: HashMap<u32, cancellation::Sender> = HashMap::new();
            let mut cancellable_requests: Vec<cancellation::Sender> = Vec::new();

            loop {
                match raw_requests_stream.recv().await {
                    Ok((seq, request)) => {
                        let sender = cancellation::Sender::new();
                        let receiver = sender.subscribe();

                        // Clean out entries which don't have any receivers.
                        pending_requests.retain(|_k, v| v.receiver_count() > 0);
                        cancellable_requests.retain(|v| v.receiver_count() > 0);

                        match request {
                            RequestArguments::cancel(args) => {
                                info!("Cancellation {:?}", args);
                                if let Some(id) = args.request_id {
                                    if let Some(sender) = pending_requests.remove(&(id as u32)) {
                                        sender.send();
                                    }
                                }
                                continue; // Dont forward to the main event loop.
                            }
                            // Requests that may be canceled.
                            RequestArguments::scopes(_)
                            | RequestArguments::variables(_)
                            | RequestArguments::evaluate(_) => cancellable_requests.push(sender.clone()),
                            // Requests that will cancel the above.
                            RequestArguments::continue_(_)
                            | RequestArguments::pause(_)
                            | RequestArguments::next(_)
                            | RequestArguments::stepIn(_)
                            | RequestArguments::stepOut(_)
                            | RequestArguments::stepBack(_)
                            | RequestArguments::reverseContinue(_)
                            | RequestArguments::terminate(_)
                            | RequestArguments::disconnect(_) => {
                                for sender in &mut cancellable_requests {
                                    sender.send();
                                }
                            }
                            _ => (),
                        }

                        pending_requests.insert(seq, sender);
                        log_errors!(requests_sender.send((seq, request, receiver)).await);
                    }
                    Err(RecvError::Lagged(count)) => error!("Missed {} messages", count),
                    Err(RecvError::Closed) => break,
                }
            }
        };
        tokio::spawn(filter);

        requests_receiver
    }

    fn with_sync_mode<F: Fn() -> R, R>(&self, func: F) -> R {
        self.debugger.set_async_mode(false);
        let result = func();
        self.debugger.set_async_mode(true);
        result
    }

    fn handle_request(&mut self, seq: u32, request_args: RequestArguments, cancellation: cancellation::Receiver) {
        if cancellation.is_cancelled() {
            self.send_response(seq, Err("canceled".into()));
        } else {
            if let Some(python) = &self.python {
                cancellation.add_callback(python.interrupt_sender())
            }
            self.current_cancellation = cancellation;
            if let RequestArguments::unknown = request_args {
                info!("Received an unknown command");
                self.send_response(seq, Err("Not implemented.".into()));
            } else {
                let result = self.handle_request_args(request_args);
                self.current_cancellation = cancellation::dummy();
                match result {
                    // Spawn async responses as tasks
                    Err(err) if err.is::<AsyncResponse>() => {
                        let self_ref = self.self_ref.clone();
                        tokio::task::spawn_local(async move {
                            let fut: std::pin::Pin<Box<_>> = err.downcast::<AsyncResponse>().unwrap().0.into();
                            let result = fut.await;
                            self_ref.map(|s| s.send_response(seq, result)).await;
                        });
                    }
                    // Send synchronous results immediately
                    _ => {
                        self.send_response(seq, result);
                    }
                }
            }
        }
    }

    #[rustfmt::skip]
    fn handle_request_args(&mut self, arguments: RequestArguments) -> Result<ResponseBody, Error> {
        match arguments {
            RequestArguments::_adapterSettings(args) =>
                self.handle_adapter_settings(args)
                    .map(|_| ResponseBody::_adapterSettings),
            RequestArguments::initialize(args) =>
                self.handle_initialize(args)
                    .map(|r| ResponseBody::initialize(r)),
            RequestArguments::launch(Either::First(args)) =>
                self.handle_launch(args),
            RequestArguments::launch(Either::Second(json)) =>
                self.report_launch_cfg_error(serde_json::from_value::<LaunchRequestArguments>(json).unwrap_err()),
            RequestArguments::attach(Either::First(args)) =>
                self.handle_attach(args),
            RequestArguments::attach(Either::Second(json)) =>
                self.report_launch_cfg_error(serde_json::from_value::<AttachRequestArguments>(json).unwrap_err()),
            RequestArguments::restart(Either::First(args)) =>
                self.handle_restart(args)
                    .map(|_| ResponseBody::restart),
            RequestArguments::restart(Either::Second(json)) =>
                self.report_launch_cfg_error(serde_json::from_value::<
                    Either<LaunchRequestArguments, AttachRequestArguments>>(json).unwrap_err()),
            RequestArguments::configurationDone(_) =>
                self.handle_configuration_done()
                    .map(|_| ResponseBody::configurationDone),
            RequestArguments::disconnect(args) =>
                self.handle_disconnect(args)
                    .map(|_| ResponseBody::disconnect),
            _ => {
                if self.no_debug {
                    bail!("Not supported in noDebug mode.")
                } else {
                    match arguments {
                        RequestArguments::setBreakpoints(args) =>
                            self.handle_set_breakpoints(args)
                                .map(|r| ResponseBody::setBreakpoints(r)),
                        RequestArguments::setInstructionBreakpoints(args) =>
                            self.handle_set_instruction_breakpoints(args)
                                .map(|r| ResponseBody::setInstructionBreakpoints(r)),
                        RequestArguments::setFunctionBreakpoints(args) =>
                            self.handle_set_function_breakpoints(args)
                                .map(|r| ResponseBody::setFunctionBreakpoints(r)),
                        RequestArguments::setExceptionBreakpoints(args) =>
                            self.handle_set_exception_breakpoints(args)
                                .map(|_| ResponseBody::setExceptionBreakpoints),
                        RequestArguments::exceptionInfo(args) =>
                            self.handle_execption_info(args)
                                .map(|r| ResponseBody::exceptionInfo(r)),
                        RequestArguments::threads(_) =>
                            self.handle_threads()
                                .map(|r| ResponseBody::threads(r)),
                        RequestArguments::stackTrace(args) =>
                            self.handle_stack_trace(args)
                                .map(|r| ResponseBody::stackTrace(r)),
                        RequestArguments::scopes(args) =>
                            self.handle_scopes(args)
                                .map(|r| ResponseBody::scopes(r)),
                        RequestArguments::variables(args) =>
                            self.handle_variables(args)
                                .map(|r| ResponseBody::variables(r)),
                        RequestArguments::evaluate(args) =>
                            self.handle_evaluate(args),
                        RequestArguments::setVariable(args) =>
                            self.handle_set_variable(args)
                                .map(|r| ResponseBody::setVariable(r)),
                        RequestArguments::pause(args) =>
                            self.handle_pause(args)
                                .map(|_| ResponseBody::pause),
                        RequestArguments::continue_(args) =>
                            self.handle_continue(args)
                                .map(|r| ResponseBody::continue_(r)),
                        RequestArguments::next(args) =>
                            self.handle_next(args)
                                .map(|_| ResponseBody::next),
                        RequestArguments::stepInTargets(args) =>
                            self.handle_step_in_targets(args)
                                .map(|r| ResponseBody::stepInTargets(r)),
                        RequestArguments::stepIn(args) =>
                            self.handle_step_in(args)
                                .map(|_| ResponseBody::stepIn),
                        RequestArguments::stepOut(args) =>
                            self.handle_step_out(args)
                                .map(|_| ResponseBody::stepOut),
                        RequestArguments::stepBack(args) =>
                            self.handle_step_back(args)
                                .map(|_| ResponseBody::stepBack),
                        RequestArguments::reverseContinue(args) =>
                            self.handle_reverse_continue(args)
                                .map(|_| ResponseBody::reverseContinue),
                        RequestArguments::source(args) =>
                            self.handle_source(args)
                                .map(|r| ResponseBody::source(r)),
                        RequestArguments::modules(args) =>
                            self.handle_modules(args)
                                .map(|r| ResponseBody::modules(r)),
                        RequestArguments::completions(args) =>
                            self.handle_completions(args)
                                .map(|r| ResponseBody::completions(r)),
                        RequestArguments::gotoTargets(args) =>
                            self.handle_goto_targets(args)
                                .map(|r| ResponseBody::gotoTargets(r)),
                        RequestArguments::goto(args) =>
                            self.handle_goto(args)
                                .map(|_| ResponseBody::goto),
                        RequestArguments::restartFrame(args) =>
                            self.handle_restart_frame(args)
                                .map(|_| ResponseBody::restartFrame),
                        RequestArguments::dataBreakpointInfo(args) =>
                            self.handle_data_breakpoint_info(args)
                                .map(|r| ResponseBody::dataBreakpointInfo(r)),
                        RequestArguments::setDataBreakpoints(args) =>
                            self.handle_set_data_breakpoints(args)
                                .map(|r| ResponseBody::setDataBreakpoints(r)),
                        RequestArguments::disassemble(args) =>
                            self.handle_disassemble(args)
                                .map(|r| ResponseBody::disassemble(r)),
                        RequestArguments::readMemory(args) =>
                            self.handle_read_memory(args)
                                .map(|r| ResponseBody::readMemory(r)),
                        RequestArguments::writeMemory(args) =>
                            self.handle_write_memory(args)
                                .map(|r| ResponseBody::writeMemory(r)),
                        RequestArguments::_symbols(args) =>
                            self.handle_symbols(args)
                                .map(|r| ResponseBody::_symbols(r)),
                        RequestArguments::_excludeCaller(args) =>
                            self.handle_exclude_caller(args)
                                .map(|r| ResponseBody::_excludeCaller(r)),
                        RequestArguments::_setExcludedCallers(args) =>
                            self.handle_set_excluded_callers(args)
                                .map(|_| ResponseBody::_setExcludedCallers),
                        RequestArguments::_pythonMessage(args) =>
                            self.handle_python_message(args)
                                .map(|_| ResponseBody::_pythonMessage),
                        _=> bail!("Not implemented.")
                    }
                }
            }
        }
    }

    fn send_response(&self, request_seq: u32, result: Result<ResponseBody, Error>) {
        let response = match result {
            Ok(body) => Response {
                request_seq: request_seq,
                success: true,
                result: ResponseResult::Success { body: body },
            },
            Err(err) => {
                let blamed = BlamedError::from(err);
                let (message, show) = match blamed.blame {
                    Blame::Internal => (format!("Internal debugger error: {}", blamed.inner), true),
                    Blame::User => (format!("{}", blamed.inner), true),
                    Blame::Nobody => (format!("{}", blamed.inner), false),
                };
                if show {
                    error!("{}", message);
                } else {
                    debug!("{}", message);
                }
                Response {
                    request_seq: request_seq,
                    success: false,
                    result: ResponseResult::Error {
                        command: String::new(),
                        message: message,
                        show_user: Some(show),
                    },
                }
            }
        };
        log_errors!(self.dap_session.try_send_response(response));
    }

    fn send_event(&self, event_body: EventBody) {
        log_errors!(self.dap_session.try_send_event(event_body));
    }

    fn console_message(&self, output: impl std::fmt::Display) {
        self.console_message_impl(Some("console"), output);
    }

    fn console_error(&self, output: impl std::fmt::Display) {
        self.console_message_impl(Some("stderr"), output);
    }

    fn console_message_impl(&self, category: Option<&str>, output: impl std::fmt::Display) {
        self.send_event(EventBody::output(OutputEventBody {
            output: format!("{}\n", output),
            category: category.map(Into::into),
            ..Default::default()
        }));
    }

    fn handle_initialize(&mut self, args: InitializeRequestArguments) -> Result<Capabilities, Error> {
        self.debugger.broadcaster().add_listener(
            &self.debugger.listener(),
            SBDebugger::BroadcastBitWarning | SBDebugger::BroadcastBitError,
        );
        self.debugger.listener().start_listening_for_event_class(
            &self.debugger,
            SBThread::broadcaster_class_name(),
            !0,
        );
        self.client_caps = Initialized(args);
        Ok(self.make_capabilities())
    }

    fn make_capabilities(&self) -> Capabilities {
        Capabilities {
            exception_breakpoint_filters: Some(self.get_exception_filters_for(&self.source_languages)),
            support_terminate_debuggee: Some(true),
            supports_cancel_request: Some(true),
            supports_clipboard_context: Some(true),
            supports_completions_request: Some(self.command_completions),
            supports_conditional_breakpoints: Some(true),
            supports_configuration_done_request: Some(true),
            supports_data_breakpoints: Some(true),
            supports_data_breakpoint_bytes: Some(true),
            supports_delayed_stack_trace_loading: Some(true),
            supports_disassemble_request: Some(true),
            supports_evaluate_for_hovers: Some(self.evaluate_for_hovers),
            supports_exception_filter_options: Some(true),
            supports_exception_info_request: Some(true),
            supports_function_breakpoints: Some(true),
            supports_goto_targets_request: Some(true),
            supports_hit_conditional_breakpoints: Some(true),
            supports_instruction_breakpoints: Some(true),
            supports_log_points: Some(true),
            supports_modules_request: Some(true),
            supports_read_memory_request: Some(true),
            supports_restart_request: Some(true),
            supports_set_variable: Some(true),
            supports_step_in_targets_request: Some(lldb_stub::v16.resolve().is_ok()),
            supports_stepping_granularity: Some(true),
            supports_write_memory_request: Some(true),
            ..Default::default()
        }
    }

    fn get_exception_filters_for(&self, source_langs: &[String]) -> Vec<ExceptionBreakpointsFilter> {
        let mut result = vec![];
        for exc_filter in DebugSession::get_exception_filters() {
            let filter_lang = exc_filter.filter.split('_').next().unwrap();
            if source_langs.iter().any(|l| l == filter_lang) {
                result.push(exc_filter.clone());
            }
        }
        result
    }

    fn exec_commands(&self, script_name: &str, commands: &[String]) -> Result<(), Error> {
        self.console_message(format!("Executing script: {}", script_name));
        let interpreter = self.debugger.command_interpreter();
        let mut cmd_result = SBCommandReturnObject::new();
        let result = (|| {
            for command in commands {
                cmd_result.clear();
                let ok = interpreter.handle_command(&command, &mut cmd_result, false);
                debug!("{} -> {:?}, {:?}", command, ok, cmd_result);
                let output = cmd_result.output().to_string_lossy().into_owned();
                if !output.is_empty() {
                    self.console_message(output);
                }
                if !cmd_result.succeeded() {
                    let err_msg = cmd_result.error().to_string_lossy();
                    self.console_error(&err_msg);
                    bail!(blame_user(str_error(err_msg)))
                }
            }
            Ok(())
        })();
        self.debugger.set_async_mode(true); // In case they changed it.
        result
    }

    fn handle_configuration_done(&mut self) -> Result<(), Error> {
        // Signal to complete pending launch/attach tasks.
        log_errors!(self.configuration_done_sender.send(()));
        let self_ref = self.self_ref.clone();
        let fut = async move {
            // Wait for them to finish before responding.
            while self_ref.map(|s| s.configuration_done_sender.receiver_count()).await > 0 {
                tokio::task::yield_now().await
            }
            Ok(ResponseBody::configurationDone)
        };
        Err(AsyncResponse(Box::new(fut)).into())
    }

    fn handle_threads(&mut self) -> Result<ThreadsResponseBody, Error> {
        let mut response = ThreadsResponseBody { threads: vec![] };
        for thread in self.target.process().threads() {
            let mut descr = format!("{}: tid={}", thread.index_id(), thread.thread_id());
            if let Some(name) = thread.name() {
                log_errors!(write!(descr, " \"{}\"", name));
            }
            response.threads.push(Thread {
                id: thread.thread_id() as i64,
                name: descr,
            });
        }
        Ok(response)
    }

    fn handle_stack_trace(&mut self, args: StackTraceArguments) -> Result<StackTraceResponseBody, Error> {
        let thread = self.thread_by_id(args.thread_id)?;
        let start_frame = args.start_frame.unwrap_or(0);
        let levels = args.levels.unwrap_or(std::i64::MAX);

        let mut stack_frames = vec![];
        for i in start_frame..(start_frame + levels) {
            let frame = thread.frame_at_index(i as u32);
            if !frame.is_valid() {
                break;
            }

            let key = format!("[{},{}]", thread.index_id(), i);
            let handle = self.var_refs.create(None, &key, Container::StackFrame(frame.clone()));

            let mut stack_frame: StackFrame = Default::default();
            stack_frame.id = handle.get() as i64;
            let pc_address = frame.pc_address();
            stack_frame.instruction_pointer_reference = Some(format!("0x{:X}", pc_address.load_address(&self.target)));
            stack_frame.name = if let Some(name) = frame.function_name() {
                name.to_owned()
            } else {
                format!("{:X}", pc_address.file_address())
            };

            let module = frame.module();
            if module.is_valid() {
                stack_frame.module_id = Some(serde_json::Value::String(self.module_id(&module)))
            }

            if !self.in_disassembly(&frame) {
                if let Some(le) = frame.line_entry() {
                    let fs = le.file_spec();
                    if let Some(local_path) = self.map_filespec_to_local(&fs) {
                        stack_frame.line = le.line() as i64;
                        stack_frame.column = le.column() as i64;
                        stack_frame.source = Some(Source {
                            name: Some(local_path.file_name().unwrap().to_string_lossy().into_owned()),
                            path: Some(local_path.to_string_lossy().into_owned()),
                            ..Default::default()
                        });
                    }
                }
            } else {
                let pc_addr = frame.pc();
                if let Ok(dasm) = self.disasm_ranges.from_address(pc_addr) {
                    stack_frame.line = dasm.line_num_by_address(pc_addr) as i64;
                    stack_frame.source = Some(Source {
                        name: Some(dasm.source_name().to_owned()),
                        source_reference: Some(handles::to_i64(Some(dasm.handle()))),
                        ..Default::default()
                    });
                }
                stack_frame.column = 0;
                // Mark disassembly frames as "subtle" to reduce visual clutter,
                // unless we are in "Always Show Disassembly" mode, or this is the first frame.
                if i > 0 && self.show_disassembly != ShowDisassembly::Always {
                    stack_frame.presentation_hint = Some("subtle".to_owned());
                }
            }
            stack_frames.push(stack_frame);
        }

        Ok(StackTraceResponseBody {
            stack_frames: stack_frames,
            total_frames: None,
        })
    }

    fn in_disassembly(&mut self, frame: &SBFrame) -> bool {
        match self.show_disassembly {
            ShowDisassembly::Always => true,
            ShowDisassembly::Never => false,
            ShowDisassembly::Auto => {
                if let Some(le) = frame.line_entry() {
                    self.map_filespec_to_local(&le.file_spec()).is_none()
                } else {
                    true
                }
            }
        }
    }

    fn handle_pause(&mut self, _args: PauseArguments) -> Result<(), Error> {
        match self.target.process().stop() {
            Ok(()) => Ok(()),
            Err(err) => {
                if !self.target.process().state().is_running() {
                    // Did we lose a 'stopped' event?
                    self.notify_process_stopped();
                    Ok(())
                } else {
                    bail!(blame_user(err.into()));
                }
            }
        }
    }

    fn handle_continue(&mut self, _args: ContinueArguments) -> Result<ContinueResponseBody, Error> {
        self.before_resume();
        let process = self.target.process();
        match process.resume() {
            Ok(()) => Ok(ContinueResponseBody {
                all_threads_continued: Some(true),
            }),
            Err(err) => {
                if process.state().is_running() {
                    // Did we lose a 'running' event?
                    self.notify_process_running();
                    Ok(ContinueResponseBody {
                        all_threads_continued: Some(true),
                    })
                } else {
                    bail!(blame_user(err.into()))
                }
            }
        }
    }

    fn handle_next(&mut self, args: NextArguments) -> Result<(), Error> {
        let thread = self.thread_by_id(args.thread_id)?;

        self.before_resume();

        let step_instruction = match args.granularity {
            Some(SteppingGranularity::Instruction) => true,
            Some(SteppingGranularity::Line) | Some(SteppingGranularity::Statement) => false,
            None => {
                let frame = thread.frame_at_index(0);
                self.in_disassembly(&frame)
            }
        };

        if step_instruction {
            thread.step_instruction(true)?;
        } else {
            thread.step_over(RunMode::OnlyDuringStepping)?;
        }
        Ok(())
    }

    fn handle_step_out(&mut self, args: StepOutArguments) -> Result<(), Error> {
        self.before_resume();
        let thread = self.thread_by_id(args.thread_id)?;
        thread.step_out()?;
        Ok(())
    }

    fn handle_step_back(&mut self, args: StepBackArguments) -> Result<(), Error> {
        self.before_resume();
        self.show_disassembly = ShowDisassembly::Always; // Reverse line-step is not supported, so we switch to disassembly mode.
        self.reverse_exec(&[
            &format!("process plugin packet send Hc{:x}", args.thread_id), // select thread
            "process plugin packet send bs",                               // reverse-step
            "process plugin packet send bs",                               // reverse-step so we can forward step
            "stepi", // forward-step to refresh LLDB's cached debuggee state
        ])
    }

    fn handle_reverse_continue(&mut self, args: ReverseContinueArguments) -> Result<(), Error> {
        self.before_resume();
        self.reverse_exec(&[
            &format!("process plugin packet send Hc{:x}", args.thread_id), // select thread
            "process plugin packet send bc",                               // reverse-continue
            "process plugin packet send bs",                               // reverse-step so we can forward step
            "stepi", // forward-step to refresh LLDB's cached debuggee state
        ])
    }

    fn reverse_exec(&mut self, commands: &[&str]) -> Result<(), Error> {
        let interp = self.debugger.command_interpreter();
        let mut result = SBCommandReturnObject::new();
        for command in commands {
            interp.handle_command(&command, &mut result, false);
            if !result.succeeded() {
                let error = into_string_lossy(result.error());
                self.console_error(error.clone());
                bail!(error);
            }
        }
        Ok(())
    }

    fn handle_source(&mut self, args: SourceArguments) -> Result<SourceResponseBody, Error> {
        let handle = handles::from_i64(args.source_reference)?;
        let dasm = self.disasm_ranges.find_by_handle(handle).unwrap();
        Ok(SourceResponseBody {
            content: dasm.get_source_text(self.max_instr_bytes),
            mime_type: Some("text/x-lldb.disassembly".to_owned()),
        })
    }

    fn handle_modules(&mut self, args: ModulesArguments) -> Result<ModulesResponseBody, Error> {
        let start = args.start_module.unwrap_or(0);
        let count = args.module_count.unwrap_or(std::i64::MAX);
        let mut modules = Vec::new();
        for i in start..start + count {
            let module = self.target.module_at_index(i as u32);
            if !module.is_valid() {
                break;
            }
            modules.push(self.make_module_detail(&module));
        }
        Ok(ModulesResponseBody {
            modules: modules,
            total_modules: Some(self.target.num_modules() as i64),
        })
    }

    fn handle_completions(&mut self, args: CompletionsArguments) -> Result<CompletionsResponseBody, Error> {
        if !self.command_completions {
            bail!("Completions are disabled");
        }
        let (text, cursor_column) = match self.console_mode {
            ConsoleMode::Commands => (&args.text[..], args.column - 1),
            ConsoleMode::Split | ConsoleMode::Evaluate => {
                if args.text.starts_with('`') {
                    (&args.text[1..], args.column - 2)
                } else if args.text.starts_with("/cmd ") {
                    (&args.text[5..], args.column - 6)
                } else {
                    // TODO: expression completions
                    return Ok(CompletionsResponseBody { targets: vec![] });
                }
            }
        };

        // Work around LLDB crash when text starts with non-alphabetic character.
        if let Some(c) = text.chars().next() {
            if !c.is_alphabetic() {
                return Ok(CompletionsResponseBody { targets: vec![] });
            }
        }

        // Compute cursor position inside text in as byte offset.
        let cursor_index = text.char_indices().skip(cursor_column as usize).next().map(|p| p.0).unwrap_or(text.len());

        let interpreter = self.debugger.command_interpreter();
        let targets = match interpreter.handle_completions(text, cursor_index as u32, None) {
            None => vec![],
            Some((common_continuation, completions)) => {
                // LLDB completions usually include some prefix of the string being completed, without telling us what that prefix is.
                // For example, completing "set show tar" might return ["target.arg0", "target.auto-apply-fixits", ...].

                // Take a slice up to the cursor, split it on whitespaces, then get the last part.
                // This is the (likely) prefix of completions returned by LLDB.
                let prefix = &text[..cursor_index].split_whitespace().next_back().unwrap_or_default();
                let prefix_len = prefix.chars().count();
                let extended_prefix = format!("{}{}", prefix, common_continuation);

                let mut targets = vec![];
                for completion in completions {
                    // Check if we guessed prefix correctly
                    let item = if completion.starts_with(&extended_prefix) {
                        CompletionItem {
                            label: completion,
                            start: Some(args.column - prefix_len as i64),
                            length: Some(prefix_len as i64),
                            ..Default::default()
                        }
                    } else {
                        // Let VSCode apply its own heuristics to figure out the prefix.
                        CompletionItem {
                            label: completion,
                            ..Default::default()
                        }
                    };
                    targets.push(item);
                }
                targets
            }
        };

        Ok(CompletionsResponseBody { targets })
    }

    fn handle_goto_targets(&mut self, args: GotoTargetsArguments) -> Result<GotoTargetsResponseBody, Error> {
        let targets = vec![GotoTarget {
            id: 1,
            label: format!("line {}", args.line),
            line: args.line,
            end_line: None,
            column: None,
            end_column: None,
            instruction_pointer_reference: None,
        }];
        self.last_goto_request = Some(args);
        Ok(GotoTargetsResponseBody { targets })
    }

    fn handle_goto(&mut self, args: GotoArguments) -> Result<(), Error> {
        match &self.last_goto_request {
            None => bail!("Unexpected goto message."),
            Some(ref goto_args) => {
                let thread = self.thread_by_id(args.thread_id)?;
                match goto_args.source.source_reference {
                    // Disassembly
                    Some(source_ref) => {
                        let handle = handles::from_i64(source_ref)?;
                        let dasm = self.disasm_ranges.find_by_handle(handle).ok_or("source_ref")?;
                        let addr = dasm.address_by_line_num(goto_args.line as u32);
                        let frame = thread.frame_at_index(0).check().ok_or("frame 0")?;
                        if frame.set_pc(addr) {
                            self.refresh_client_display(Some(args.thread_id as ThreadID));
                            Ok(())
                        } else {
                            bail!(blame_user(str_error("Failed to set the instruction pointer.")));
                        }
                    }
                    // Normal source file
                    None => {
                        let filespec = SBFileSpec::from(goto_args.source.path.as_ref().ok_or("source.path")?);
                        match thread.jump_to_line(&filespec, goto_args.line as u32) {
                            Ok(()) => {
                                self.last_goto_request = None;
                                self.refresh_client_display(Some(args.thread_id as ThreadID));
                                Ok(())
                            }
                            Err(err) => {
                                bail!(blame_user(err.into()))
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_restart_frame(&mut self, args: RestartFrameArguments) -> Result<(), Error> {
        let handle = handles::from_i64(args.frame_id)?;
        let frame = match self.var_refs.get(handle) {
            Some(Container::StackFrame(ref f)) => f.clone(),
            _ => bail!("Invalid frameId"),
        };
        let thread = frame.thread();
        thread.return_from_frame(&frame)?;
        self.send_event(EventBody::stopped(StoppedEventBody {
            thread_id: Some(thread.thread_id() as i64),
            all_threads_stopped: Some(true),
            reason: "restart".into(),
            ..Default::default()
        }));
        Ok(())
    }

    fn handle_data_breakpoint_info(
        &mut self,
        args: DataBreakpointInfoArguments,
    ) -> Result<DataBreakpointInfoResponseBody, Error> {
        if let Some(variables_reference) = args.variables_reference {
            let container_handle = handles::from_i64(variables_reference)?;
            let container = self.var_refs.get(container_handle).expect("Invalid variables reference");
            let child = match container {
                Container::SBValue(container) => container.child_member_with_name(&args.name),
                Container::Locals(frame) => frame.find_variable(&args.name),
                Container::Globals(frame) => frame.find_value(&args.name, ValueType::VariableGlobal),
                Container::Statics(frame) => frame.find_value(&args.name, ValueType::VariableStatic),
                _ => None,
            };
            if let Some(child) = child {
                let addr = child.load_address();
                if addr != lldb::INVALID_ADDRESS {
                    let size = args.bytes.unwrap_or(child.byte_size() as i64) as usize;
                    let data_id = format!("{}/{}", addr, size);
                    let desc = child.name().unwrap_or("");
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: Some(data_id),
                        access_types: Some(vec![
                            DataBreakpointAccessType::Read,
                            DataBreakpointAccessType::Write,
                            DataBreakpointAccessType::ReadWrite,
                        ]),
                        description: format!("{} bytes at {:X} ({})", size, addr, desc),
                        ..Default::default()
                    })
                } else {
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: None,
                        description: "This variable doesn't have an address.".into(),
                        ..Default::default()
                    })
                }
            } else {
                Ok(DataBreakpointInfoResponseBody {
                    data_id: None,
                    description: "Variable not found.".into(),
                    ..Default::default()
                })
            }
        } else {
            let frame = match args.frame_id {
                Some(frame_id) => {
                    let handle = handles::from_i64(frame_id)?;
                    match self.var_refs.get(handle) {
                        Some(Container::StackFrame(ref frame)) => {
                            // If they had used `frame select` command after the last stop,
                            // use currently selected frame from frame's thread, instead of the frame itself.
                            if self.selected_frame_changed {
                                Some(frame.thread().selected_frame())
                            } else {
                                Some(frame.clone())
                            }
                        }
                        _ => None,
                    }
                }
                None => None,
            };
            if args.as_address.unwrap_or(false) {
                // name is an address
                let addr = parse_int::parse::<u64>(&args.name).unwrap_or(lldb::INVALID_ADDRESS);
                let size = args.bytes.unwrap_or(self.target.address_byte_size() as i64) as usize;
                if addr == lldb::INVALID_ADDRESS || !SBAddress::from_load_address(addr, &self.target).is_valid() {
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: None,
                        description: format!("Invalid address {}", addr),
                        ..Default::default()
                    })
                } else {
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: Some(format!("{}/{}", addr, size)),
                        access_types: Some(vec![
                            DataBreakpointAccessType::Read,
                            DataBreakpointAccessType::Write,
                            DataBreakpointAccessType::ReadWrite,
                        ]),
                        description: format!("{} bytes at {:X}", size, addr),
                        ..Default::default()
                    })
                }
            } else {
                // Otherwise name is an expression
                let (pp_expr, _format_spec) =
                    expressions::prepare_with_format(&args.name, self.default_expr_type).map_err(blame_user)?;
                let value = self.evaluate_expr_in_frame(&pp_expr, frame.as_ref())?;
                let addr = value.load_address();
                if addr != lldb::INVALID_ADDRESS {
                    let size = args.bytes.unwrap_or(value.byte_size() as i64) as usize;
                    let data_id = format!("{}/{}", addr, size);
                    let desc = value.name().unwrap_or(&args.name);
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: Some(data_id),
                        access_types: Some(vec![
                            DataBreakpointAccessType::Read,
                            DataBreakpointAccessType::Write,
                            DataBreakpointAccessType::ReadWrite,
                        ]),
                        description: format!("{} bytes at {:X} ({})", size, addr, desc),
                        ..Default::default()
                    })
                } else {
                    Ok(DataBreakpointInfoResponseBody {
                        data_id: None,
                        description: "This value doesn't have an address.".into(),
                        ..Default::default()
                    })
                }
            }
        }
    }

    fn handle_set_data_breakpoints(
        &mut self,
        args: SetDataBreakpointsArguments,
    ) -> Result<SetDataBreakpointsResponseBody, Error> {
        self.target.delete_all_watchpoints();
        let mut watchpoints = vec![];
        for wp in args.breakpoints {
            let mut parts = wp.data_id.split('/');
            let addr = parts.next().ok_or("")?.parse::<u64>()?;
            let size = parts.next().ok_or("")?.parse::<usize>()?;
            let (read, write, when) = match wp.access_type {
                Some(DataBreakpointAccessType::Read) => (true, false, "read"),
                Some(DataBreakpointAccessType::Write) | None => (false, true, "write"),
                Some(DataBreakpointAccessType::ReadWrite) => (true, true, "read and write"),
            };
            let res = match self.target.watch_address(addr, size, read, write) {
                Ok(wp) => Breakpoint {
                    verified: true,
                    id: Some(DebugSession::wpid_to_bpid(wp.id())),
                    message: Some(format!("Break on {}", when)),
                    ..Default::default()
                },
                Err(err) => Breakpoint {
                    verified: false,
                    message: Some(err.to_string()),
                    ..Default::default()
                },
            };
            watchpoints.push(res);
        }
        Ok(SetDataBreakpointsResponseBody {
            breakpoints: watchpoints,
        })
    }

    // Merge watchpoint ids into breakpoint ids namespace.
    fn wpid_to_bpid(id: WatchpointID) -> i64 {
        // Avoid collision with regular breakpoints; let's hope 1M breakpoints is "enough for everyone".
        id as i64 + 1_000_000
    }

    fn handle_read_memory(&mut self, args: ReadMemoryArguments) -> Result<ReadMemoryResponseBody, Error> {
        let mem_ref = parse_int::parse::<i64>(&args.memory_reference)?;
        let offset = args.offset.unwrap_or(0);
        let count = args.count as usize;
        let address = (mem_ref + offset) as lldb::Address;
        let process = self.target.process();
        if let Ok(region_info) = process.memory_region_info(address) {
            if region_info.is_readable() {
                let to_read = cmp::min(count, (region_info.region_end() - address) as usize);
                let mut buffer = Vec::new();
                buffer.resize(to_read, 0);
                if let Ok(bytes_read) = process.read_memory(address, buffer.as_mut_slice()) {
                    buffer.resize(bytes_read, 0);
                    return Ok(ReadMemoryResponseBody {
                        address: format!("0x{:X}", address),
                        unreadable_bytes: Some((count - bytes_read) as i64),
                        data: Some(base64::encode(buffer)),
                    });
                }
            }
        }
        Ok(ReadMemoryResponseBody {
            address: format!("0x{:X}", address),
            unreadable_bytes: Some(args.count),
            data: None,
        })
    }

    fn handle_write_memory(&mut self, args: WriteMemoryArguments) -> Result<WriteMemoryResponseBody, Error> {
        let mem_ref = parse_int::parse::<i64>(&args.memory_reference)?;
        let offset = args.offset.unwrap_or(0);
        let address = (mem_ref + offset) as lldb::Address;
        let data = base64::decode(&args.data)?;
        let allow_partial = args.allow_partial.unwrap_or(false);
        let process = self.target.process();
        if let Ok(region_info) = process.memory_region_info(address) {
            if region_info.is_writable() {
                let to_write = cmp::min(data.len(), (region_info.region_end() - address) as usize);
                if allow_partial || to_write == data.len() {
                    if let Ok(bytes_written) = process.write_memory(address, &data) {
                        return Ok(WriteMemoryResponseBody {
                            bytes_written: Some(bytes_written as i64),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        if !allow_partial {
            bail!(blame_user(str_error(format!(
                "Cannot write {} bytes at {:08X}",
                data.len(),
                address
            ))));
        }
        Ok(WriteMemoryResponseBody {
            bytes_written: Some(0),
            ..Default::default()
        })
    }

    fn handle_symbols(&mut self, args: SymbolsRequest) -> Result<SymbolsResponse, Error> {
        use fuzzy_matcher::FuzzyMatcher;
        let matcher = fuzzy_matcher::clangd::ClangdMatcher::default().ignore_case();
        let mut symbols = vec![];
        'outer: for imodule in 0..self.target.num_modules() {
            let module = self.target.module_at_index(imodule);
            for isymbol in 0..module.num_symbols() {
                let symbol = module.symbol_at_index(isymbol);
                let ty = symbol.symbol_type();
                match ty {
                    SymbolType::Code | SymbolType::Data => {
                        let name = symbol.display_name();
                        if let Some(_) = matcher.fuzzy_match(name, &args.filter) {
                            let start_addr = symbol.start_address().load_address(&self.target);

                            let location = if let Some(le) = symbol.start_address().line_entry() {
                                let fs = le.file_spec();
                                if let Some(local_path) = self.map_filespec_to_local(&fs) {
                                    let source = Source {
                                        name: Some(local_path.file_name().unwrap().to_string_lossy().into_owned()),
                                        path: Some(local_path.to_string_lossy().into_owned()),
                                        ..Default::default()
                                    };
                                    Some((source, le.line()))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let symbol = Symbol {
                                name: name.into(),
                                type_: format!("{:?}", ty),
                                address: format!("0x{:X}", start_addr),
                                location: location,
                            };
                            symbols.push(symbol);
                        }
                    }
                    _ => {}
                }

                if symbols.len() >= args.max_results as usize {
                    break 'outer;
                }
            }
        }
        Ok(SymbolsResponse { symbols })
    }

    fn handle_python_message(&mut self, args: serde_json::value::Value) -> Result<(), Error> {
        if let Some(python) = &self.python {
            let body_json = args.to_string();
            python.handle_message(&body_json);
        }
        Ok(())
    }

    fn handle_python_event(&mut self, event: PythonEvent) {
        match event {
            PythonEvent::SendDapEvent(event) => {
                log_errors!(self.dap_session.try_send_event(event));
            }
            PythonEvent::DebuggerMessage { output, category } => {
                self.console_message_impl(Some(&category), output);
            }
        }
    }

    fn handle_adapter_settings(&mut self, args: AdapterSettings) -> Result<(), Error> {
        let old_console_mode = self.console_mode;
        self.update_adapter_settings_and_caps(&args);
        if self.console_mode != old_console_mode {
            self.print_console_mode();
        }
        if !self.target.process().state().is_running() {
            self.refresh_client_display(None);
        }
        Ok(())
    }

    fn update_adapter_settings_and_caps(&mut self, settings: &AdapterSettings) {
        let new_caps = self.update_adapter_settings(&settings);
        if new_caps != Default::default() {
            self.send_event(EventBody::capabilities(CapabilitiesEventBody {
                capabilities: new_caps,
            }));
        }
    }

    // Returns capabilities that changed, if any.
    fn update_adapter_settings(&mut self, settings: &AdapterSettings) -> Capabilities {
        self.global_format = match settings.display_format {
            None => self.global_format,
            Some(DisplayFormat::Auto) => Format::Default,
            Some(DisplayFormat::Decimal) => Format::Decimal,
            Some(DisplayFormat::Hex) => Format::Hex,
            Some(DisplayFormat::Binary) => Format::Binary,
        };
        self.show_disassembly = settings.show_disassembly.unwrap_or(self.show_disassembly);
        self.deref_pointers = settings.dereference_pointers.unwrap_or(self.deref_pointers);
        self.suppress_missing_files = settings.suppress_missing_source_files.unwrap_or(self.suppress_missing_files);

        if let Some(timeout) = settings.evaluation_timeout {
            self.evaluation_timeout = time::Duration::from_millis((timeout * 1000.0) as u64);
        }
        if let Some(timeout) = settings.summary_timeout {
            self.summary_timeout = time::Duration::from_millis((timeout * 1000.0) as u64);
        }
        if let Some(console_mode) = settings.console_mode {
            self.console_mode = console_mode;
        }
        let mut caps = Capabilities::default();
        if let Some(evaluate_for_hovers) = settings.evaluate_for_hovers {
            if self.evaluate_for_hovers != evaluate_for_hovers {
                self.evaluate_for_hovers = evaluate_for_hovers;
                caps.supports_evaluate_for_hovers = Some(evaluate_for_hovers);
            }
        }
        if let Some(command_completions) = settings.command_completions {
            if self.command_completions != command_completions {
                self.command_completions = command_completions;
                caps.supports_completions_request = Some(command_completions);
            }
        }
        if let Some(ref source_languages) = settings.source_languages {
            if self.source_languages.iter().ne(source_languages) {
                self.source_languages = source_languages.to_owned();
                caps.exception_breakpoint_filters = Some(self.get_exception_filters_for(&self.source_languages));
            }
        }
        if let Some(python) = &self.python {
            log_errors!(python.update_adapter_settings(settings));
        }
        caps
    }

    // Send a fake stop event to force VSCode to refresh its UI state.
    fn refresh_client_display(&mut self, thread_id: Option<ThreadID>) {
        let thread_id = match thread_id {
            Some(tid) => tid,
            None => self.target.process().selected_thread().thread_id(),
        };
        if self.client_caps.supports_invalidated_event.unwrap_or(false) {
            self.send_event(EventBody::invalidated(InvalidatedEventBody {
                thread_id: Some(thread_id as i64),
                ..Default::default()
            }));
        }
        self.send_event(EventBody::stopped(StoppedEventBody {
            thread_id: Some(thread_id as i64),
            all_threads_stopped: Some(true),
            ..Default::default()
        }));
    }

    fn before_resume(&mut self) {
        self.var_refs.reset();
        self.selected_frame_changed = false;
        self.last_goto_request = None;
        self.step_in_targets.clear();
    }

    fn handle_debug_event(&mut self, event: SBEvent) {
        debug!("Debug event: {:?}", event);
        if let Some(process_event) = event.as_process_event() {
            self.handle_process_event(&process_event);
        } else if let Some(target_event) = event.as_target_event() {
            self.handle_target_event(&target_event);
        } else if let Some(bp_event) = event.as_breakpoint_event() {
            self.handle_breakpoint_event(&bp_event);
        } else if let Some(thread_event) = event.as_thread_event() {
            self.handle_thread_event(&thread_event);
        } else if let Some(diag_event) = event.as_diagnostic_event() {
            self.handle_diagnostic_event(&diag_event);
        }
    }

    fn handle_diagnostic_event(&mut self, event: &SBStructuredData) {
        let diag_type = event.value_for_key("type").string_value();
        let message = event.value_for_key("message").string_value();
        self.console_message(format!("{diag_type}: {message}"));
    }

    fn handle_process_event(&mut self, process_event: &SBProcessEvent) {
        let event_type = process_event.as_event().event_type();
        let process = self.target.process();
        if event_type & SBProcess::BroadcastBitStateChanged != 0 {
            use ProcessState::*;
            match process_event.process_state() {
                Running | Stepping | Attaching | Launching => self.notify_process_running(),
                Stopped => {
                    if !process_event.restarted() {
                        self.notify_process_stopped()
                    }
                }
                Crashed | Suspended => self.notify_process_stopped(),
                Exited | Detached => self.notify_process_terminated(),
                Invalid | Unloaded | Connected => (),
            }
        }
        if event_type & (SBProcess::BroadcastBitSTDOUT | SBProcess::BroadcastBitSTDERR) != 0 {
            let read_stdout = |b: &mut [u8]| process.read_stdout(b);
            let read_stderr = |b: &mut [u8]| process.read_stderr(b);
            let (read_stream, category): (&dyn for<'r> Fn(&mut [u8]) -> usize, &str) =
                if event_type & SBProcess::BroadcastBitSTDOUT != 0 {
                    (&read_stdout, "stdout")
                } else {
                    (&read_stderr, "stderr")
                };
            let mut buffer = [0; 1024];
            let mut read = read_stream(&mut buffer);
            while read > 0 {
                self.send_event(EventBody::output(OutputEventBody {
                    category: Some(category.to_owned()),
                    output: String::from_utf8_lossy(&buffer[..read]).into_owned(),
                    ..Default::default()
                }));
                read = read_stream(&mut buffer);
            }
        }
    }

    fn notify_process_running(&mut self) {
        let thread_id = self.target.process().thread_at_index(0).thread_id();
        self.send_event(EventBody::continued(ContinuedEventBody {
            all_threads_continued: Some(true),
            thread_id: thread_id as i64,
        }));
    }

    fn notify_process_stopped(&mut self) {
        let process = self.target.process();
        // Check the currently selected thread first.
        let mut stopped_thread = process.selected_thread();
        let is_valid_reason = |r| r != StopReason::Invalid && r != StopReason::None;
        // Fall back to scanning all threads in the process.
        if !is_valid_reason(stopped_thread.stop_reason()) {
            for thread in process.threads() {
                if is_valid_reason(thread.stop_reason()) {
                    process.set_selected_thread(&thread);
                    stopped_thread = thread;
                    break;
                }
            }
        };

        // Analyze stop reason
        let (stop_reason, description, hit_breakpoint) = match stopped_thread.stop_reason() {
            StopReason::Breakpoint => {
                let bp_id = stopped_thread.stop_reason_data_at_index(0);
                ("breakpoint", None, Some(vec![bp_id as i64]))
            },
            StopReason::Watchpoint => {
                let wp_id = stopped_thread.stop_reason_data_at_index(0) as WatchpointID;
                ("data breakpoint", None, Some(vec![DebugSession::wpid_to_bpid(wp_id)]))
            },
            StopReason::Trace | //.
            StopReason::PlanComplete => ("step", None, None),
            StopReason::Signal => ("exception", Some(stopped_thread.stop_description()), None),
            StopReason::Exception => ("exception", Some(stopped_thread.stop_description()), None),
            _ => ("unknown", Some(stopped_thread.stop_description()), None),
        };
        let description = description.filter(|s| !s.is_empty());

        if let Some(description) = &description {
            self.console_error(format!("Stop reason: {}", description));
        }

        self.send_event(EventBody::stopped(StoppedEventBody {
            all_threads_stopped: Some(true),
            thread_id: Some(stopped_thread.thread_id() as i64),
            reason: stop_reason.to_owned(),
            description: description,
            preserve_focus_hint: None,
            hit_breakpoint_ids: hit_breakpoint,
            ..Default::default()
        }));
    }

    fn notify_process_terminated(&mut self) {
        use ProcessState::*;
        let process = self.target.process();
        match process.state() {
            Exited => {
                let exit_code = process.exit_status() as i64;
                self.console_message(format!("Process exited with code {}.", exit_code));
                self.send_event(EventBody::exited(ExitedEventBody { exit_code }));
                self.send_event(EventBody::terminated(TerminatedEventBody { restart: None }));
            }
            Detached => {
                self.console_message("Detached from debuggee.");
                self.send_event(EventBody::terminated(TerminatedEventBody { restart: None }));
            }
            _ => (),
        }
    }

    fn handle_execption_info(&mut self, args: ExceptionInfoArguments) -> Result<ExceptionInfoResponseBody, Error> {
        let thread = self.thread_by_id(args.thread_id)?;
        let einfo = ExceptionInfoResponseBody {
            exception_id: format!("{:?}", thread.stop_reason()),
            description: Some(thread.stop_description()),
            break_mode: ExceptionBreakMode::Always,
            details: None,
        };
        Ok(einfo)
    }

    fn handle_target_event(&mut self, event: &SBTargetEvent) {
        let event_type = event.as_event().event_type();
        if event_type & SBTarget::BroadcastBitModulesLoaded != 0 {
            for module in event.modules() {
                self.send_event(EventBody::module(ModuleEventBody {
                    reason: "new".to_owned(),
                    module: self.make_module_detail(&module),
                }));
            }
        } else if event_type & SBTarget::BroadcastBitSymbolsLoaded != 0 {
            for module in event.modules() {
                self.send_event(EventBody::module(ModuleEventBody {
                    reason: "changed".to_owned(),
                    module: self.make_module_detail(&module),
                }));
            }
        } else if event_type & SBTarget::BroadcastBitModulesUnloaded != 0 {
            for module in event.modules() {
                self.send_event(EventBody::module(ModuleEventBody {
                    reason: "removed".to_owned(),
                    module: Module {
                        id: serde_json::Value::String(self.module_id(&module)),
                        ..Default::default()
                    },
                }));
            }
        }
    }

    fn module_id(&self, module: &SBModule) -> String {
        let header_addr = module.object_header_address();
        if header_addr.is_valid() {
            format!("{:X}", header_addr.load_address(&self.target))
        } else {
            // header_addr not available on Windows, fall back to path
            module.file_spec().path().display().to_string()
        }
    }

    fn make_module_detail(&self, module: &SBModule) -> Module {
        let mut msg = Module {
            id: serde_json::Value::String(self.module_id(&module)),
            name: module.file_spec().filename().display().to_string(),
            path: Some(module.file_spec().path().display().to_string()),
            ..Default::default()
        };

        let header_addr = module.object_header_address();
        if header_addr.is_valid() {
            msg.address_range = Some(format!("{:X}", header_addr.load_address(&self.target)));
        }

        let symbols = module.symbol_file_spec();
        if symbols.is_valid() {
            msg.symbol_status = Some("Symbols loaded.".into());
            msg.symbol_file_path = Some(module.symbol_file_spec().path().display().to_string());
        } else {
            msg.symbol_status = Some("Symbols not found".into())
        }

        msg
    }

    fn handle_thread_event(&mut self, event: &SBThreadEvent) {
        let event_type = event.as_event().event_type();
        if event_type & SBThread::BroadcastBitSelectedFrameChanged != 0 {
            self.selected_frame_changed = true;
        }
    }

    // Maps remote file path to local file path.
    // The bulk of this work is done by LLDB itself (via target.source-map), in addition to which:
    // - if `filespec` contains a relative path, we convert it to an absolute one using relative_path_base
    //   (which is normally initialized to ${workspaceFolder}) as a base.
    // - we check whether the local file actually exists, and suppress it (if `suppress_missing_files` is true),
    //   to prevent VSCode from prompting to create them.
    fn map_filespec_to_local(&self, filespec: &SBFileSpec) -> Option<Rc<PathBuf>> {
        if !filespec.is_valid() {
            return None;
        } else {
            let source_path = filespec.path();
            let mut source_map_cache = self.source_map_cache.borrow_mut();
            match source_map_cache.get(&source_path) {
                Some(mapped_path) => mapped_path.clone(),
                None => {
                    let mut path = filespec.path();
                    // Make sure the path is absolute.
                    if path.is_relative() {
                        path = self.relative_path_base.join(path);
                    }
                    path = normalize_path(path);
                    // VSCode sometimes fails to compare equal paths that differ in casing.
                    let mapped_path = match get_fs_path_case(&path) {
                        Ok(path) if path.is_file() => Some(Rc::new(path)),
                        _ => {
                            if self.suppress_missing_files {
                                None
                            } else {
                                Some(Rc::new(path))
                            }
                        }
                    };
                    // Cache the result, so we don't have to probe file system again for the same path.
                    source_map_cache.insert(source_path, mapped_path.clone());
                    mapped_path
                }
            }
        }
    }

    fn context_from_frame(&self, frame: Option<&SBFrame>) -> SBExecutionContext {
        match frame {
            Some(frame) => SBExecutionContext::from_frame(&frame),
            None => {
                let target = self.debugger.selected_target();
                let process = target.process();
                if process.is_valid() {
                    let thread = process.selected_thread();
                    SBExecutionContext::from_thread(&thread)
                } else {
                    SBExecutionContext::from_target(&target)
                }
            }
        }
    }

    fn thread_by_id(&self, thread_id: i64) -> Result<SBThread, Error> {
        let process = self.target.process();
        match process.thread_by_id(thread_id as ThreadID) {
            Some(thread) => Ok(thread),
            None => {
                if process.state().is_running() {
                    bail!(blame_nobody(str_error("Not available while the process is running.")))
                } else {
                    bail!(str_error("Invalid thread_id."));
                }
            }
        }
    }
}

impl Drop for DebugSession {
    fn drop(&mut self) {
        debug!("DebugSession::drop()");
    }
}

fn into_string_lossy(cstr: &CStr) -> String {
    cstr.to_string_lossy().into_owned()
}
