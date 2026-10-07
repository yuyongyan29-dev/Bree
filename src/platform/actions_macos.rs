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
}
impl NativeBackend {
    pub(super) fn new() -> Result<Self, String> {
        Ok(Self {
            collector: Collector::new()?,
            handles: Vec::new(),
        })
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
        enabled()
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.collector.snapshot()
    }
    fn freeze(&mut self, identity: &ProcessIdentity, scope: &AppScope) -> Result<usize, String> {
        Self::main_thread()?;
        if !enabled() {
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
        if !enabled() {
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
