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
            return Err("原生控制对象只允许在主线程使用".into());
        }
        Ok(())
    }
    fn check_application(
        app: &NSRunningApplication,
        scope: &AppScope,
        identity: &ProcessIdentity,
    ) -> Result<(), String> {
        if app.isTerminated() || app.processIdentifier() != identity.pid as i32 {
            return Err("冻结应用已退出或实例不匹配".into());
        }
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return Err("对象不是普通前台可见 Mac 应用".into());
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
            return Err("冻结应用的安装范围不匹配".into());
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
            .ok_or("旧实例已退出、开始标记变化或不可读")?;
        if process.uid != Some(unsafe { libc::geteuid() })
            || process.executable_path.as_deref() != Some(&scope.executable_path)
            || process.id != identity.object_id()
        {
            return Err("执行用户、路径或实例不匹配".into());
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
                .ok_or("没有可靠的应用实例")?;
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
        let target = self.handles.get(handle).ok_or("冻结控制对象不存在")?;
        if target.identity != *identity || target.scope != *scope {
            return Err("冻结控制对象不匹配，不重新绑定 PID".into());
        }
        Self::check_process(&current, identity, scope)?;
        Self::check_application(&target.app, scope, identity)?;
        let front = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .ok_or("前台状态不可读取，跳过请求")?;
        if front == target.app || target.app.isActive() {
            return Err("请求前目标位于前台，保护跳过".into());
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("正常退出递送前已取消".into());
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
            return Observation::Unknown("原控制对象缺失".into());
        };
        if target.identity != *identity || target.scope != *scope {
            return Observation::Unknown("冻结对象不匹配".into());
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
            Observation::Unknown("内核实例与应用退出观察尚未一致；不能声称成功".into())
        }
    }
}
