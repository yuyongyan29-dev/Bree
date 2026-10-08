//! Native handles stay on the main thread and represent one NSRunningApplication.
//! This adapter is compiled but cannot issue requests while the A1 gate is closed.
use super::{AppScope, Backend, Collector, Observation, ProcessIdentity, Snapshot, enabled};
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use std::sync::atomic::{AtomicBool, Ordering};

struct Handle {
    app: Retained<NSRunningApplication>,
    identity: ProcessIdentity,
    scope: AppScope,
    original_instances: Vec<ProcessIdentity>,
}
pub(super) struct NativeBackend {
    collector: Collector,
    handles: Vec<Handle>,
    // Compiled only into the isolated P2 test library, never the CLI library.
    #[cfg(test)]
    fixture: Option<(ProcessIdentity, AppScope)>,
}
impl NativeBackend {
    pub(super) fn new() -> Result<Self, String> {
        Ok(Self {
            collector: Collector::new()?,
            handles: Vec::new(),
            #[cfg(test)]
            fixture: None,
        })
    }
    #[cfg(test)]
    pub(super) fn new_fixture(identity: ProcessIdentity, scope: AppScope) -> Result<Self, String> {
        Self::main_thread()?;
        scope.validate()?;
        let root = std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(".artifacts/p2")
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let path = std::path::Path::new(&scope.executable_path);
        let run_name = path
            .ancestors()
            .nth(5)
            .and_then(|root| root.file_name())
            .and_then(|name| name.to_str())
            .ok_or("Missing P2 experiment directory identity")?;
        if scope.bundle_id != format!("local.bree.p2.quit-fixture.t{run_name}")
            || !path.starts_with(root)
            || !path.ends_with("BreeQuitFixture.app/Contents/MacOS/BreeQuitFixture")
            || identity.status != crate::model::Validity::Ok
            || identity.start_seconds.is_none()
            || identity.start_microseconds.is_none()
        {
            return Err("P2 permits only the self-built fixture with complete identity".into());
        }
        let mut backend = Self::new()?;
        backend.fixture = Some((identity, scope));
        Ok(backend)
    }
    fn control_enabled(&self, identity: &ProcessIdentity, scope: &AppScope) -> bool {
        #[cfg(test)]
        if self
            .fixture
            .as_ref()
            .is_some_and(|(id, installation)| id == identity && installation == scope)
        {
            return true;
        }
        let _ = (identity, scope);
        enabled()
    }
    fn main_thread() -> Result<(), String> {
        if unsafe { libc::pthread_main_np() } == 0 {
            return Err("Native control objects may only be used on the main thread".into());
        }
        Ok(())
    }
    fn check_application(
        app: &NSRunningApplication,
        scope: &AppScope,
        identity: &ProcessIdentity,
    ) -> Result<(), String> {
        if app.isTerminated() || app.processIdentifier() != identity.pid as i32 {
            return Err("The frozen app exited or its instance does not match".into());
        }
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return Err("The object is not an ordinary user-visible Mac app".into());
        }
        if app.bundleIdentifier().map(|s| s.to_string()).as_deref() != Some(&scope.bundle_id)
            || app
                .bundleURL()
                .and_then(|u| u.path())
                .map(|s| s.to_string())
                .as_deref()
                != Some(&scope.bundle_path)
            || app
                .executableURL()
                .and_then(|u| u.path())
                .map(|s| s.to_string())
                .as_deref()
                != Some(&scope.executable_path)
        {
            return Err("The frozen app installation scope does not match".into());
        }
        Ok(())
    }
    fn check_process(
        snapshot: &Snapshot,
        identity: &ProcessIdentity,
        scope: &AppScope,
    ) -> Result<(), String> {
        let process = snapshot
            .processes
            .iter()
            .find(|p| p.identity == *identity)
            .ok_or("The old instance exited, its start identity changed, or it is unreadable")?;
        if process.uid != Some(unsafe { libc::geteuid() })
            || process.executable_path.as_deref() != Some(&scope.executable_path)
            || process.id != identity.object_id()
        {
            return Err("The execution user, path, or instance does not match".into());
        }
        Ok(())
    }
}
impl Backend for NativeBackend {
    fn pump(&mut self) {
        crate::collect::pump_platform_events();
    }
    fn available(&self) -> bool {
        #[cfg(test)]
        if self.fixture.is_some() {
            return true;
        }
        enabled()
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let sample = self.collector.snapshot()?;
        #[cfg(test)]
        {
            let mut sample = sample;
            if let Some((identity, scope)) = &self.fixture {
                for process in &mut sample.processes {
                    if process.identity == *identity
                        && process.executable_path.as_deref() == Some(&scope.executable_path)
                    {
                        process.quit_supported = true;
                    }
                }
            }
            Ok(sample)
        }
        #[cfg(not(test))]
        Ok(sample)
    }
    fn freeze(&mut self, identity: &ProcessIdentity, scope: &AppScope) -> Result<usize, String> {
        Self::main_thread()?;
        if !self.control_enabled(identity, scope) {
            return Err(super::capability_reason().into());
        }
        scope.validate()?;
        let sample = self.collector.snapshot()?;
        Self::check_process(&sample, identity, scope)?;
        // PID lookup occurs only during freeze, between two complete identity checks.
        let app =
            NSRunningApplication::runningApplicationWithProcessIdentifier(identity.pid as i32)
                .ok_or("No reliable app instance")?;
        Self::check_application(&app, scope, identity)?;
        Self::check_process(&self.collector.snapshot()?, identity, scope)?;
        let index = self.handles.len();
        self.handles.push(Handle {
            app,
            identity: identity.clone(),
            scope: scope.clone(),
            original_instances: sample
                .processes
                .iter()
                .filter(|p| p.executable_path.as_deref() == Some(&scope.executable_path))
                .map(|p| p.identity.clone())
                .collect(),
        });
        Ok(index)
    }
    fn request(
        &mut self,
        handle: usize,
        identity: &ProcessIdentity,
        scope: &AppScope,
        cancelled: &AtomicBool,
    ) -> Result<bool, String> {
        Self::main_thread()?;
        if !self.control_enabled(identity, scope) {
            return Err(super::capability_reason().into());
        }
        crate::collect::pump_platform_events();
        let current = self.collector.snapshot()?;
        let target = self
            .handles
            .get(handle)
            .ok_or("The frozen control object does not exist")?;
        if target.identity != *identity || target.scope != *scope {
            return Err("The frozen control object does not match; the PID is not rebound".into());
        }
        Self::check_process(&current, identity, scope)?;
        Self::check_application(&target.app, scope, identity)?;
        let front = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .ok_or("Foreground status is unreadable; request skipped")?;
        if front == target.app || target.app.isActive() {
            return Err(
                "The target is in the foreground before the request; skipped for protection".into(),
            );
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("Cancelled before normal termination delivery".into());
        }
        // The receiver is the frozen retained instance. No PID lookup or signal fallback.
        Ok(target.app.terminate())
    }
    fn observe(
        &mut self,
        handle: usize,
        identity: &ProcessIdentity,
        scope: &AppScope,
    ) -> Observation {
        if let Err(error) = Self::main_thread() {
            return Observation::Unknown(error);
        }
        let Some(target) = self.handles.get(handle) else {
            return Observation::Unknown("The original control object is missing".into());
        };
        if target.identity != *identity || target.scope != *scope {
            return Observation::Unknown("The frozen object does not match".into());
        }
        let sample = match self.collector.snapshot() {
            Ok(sample) => sample,
            Err(error) => return Observation::Unknown(error),
        };
        let original_alive = sample.processes.iter().any(|p| p.identity == *identity);
        if target.app.isTerminated() && !original_alive {
            let restarted = sample.processes.iter().any(|p| {
                p.identity != *identity
                    && !target.original_instances.contains(&p.identity)
                    && p.executable_path.as_deref() == Some(&scope.executable_path)
            });
            Observation::Exited { restarted }
        } else if original_alive && !target.app.isTerminated() {
            Observation::Running
        } else {
            Observation::Unknown("Kernel instance and app exit observations do not yet agree; success cannot be claimed".into())
        }
    }
}
