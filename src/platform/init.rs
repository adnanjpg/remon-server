#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum InitSystem {
    Systemd,
    OpenRc,
    SysV,
    WindowsScm,
    Unknown,
}

pub fn detect() -> InitSystem {
    #[cfg(target_os = "linux")]
    {
        // /run/systemd/private exists only when systemd is PID 1 and running.
        // /sys/fs/cgroup/systemd is an older fallback (cgroup v1 hierarchies).
        if std::path::Path::new("/run/systemd/private").exists()
            || std::path::Path::new("/sys/fs/cgroup/systemd").exists()
        {
            return InitSystem::Systemd;
        }
        // OpenRC leaves /run/openrc/ and ships /sbin/openrc.
        if std::path::Path::new("/run/openrc").exists()
            || std::path::Path::new("/sbin/openrc").exists()
        {
            return InitSystem::OpenRc;
        }
        return InitSystem::SysV;
    }

    #[cfg(target_os = "windows")]
    {
        return InitSystem::WindowsScm;
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    InitSystem::Unknown
}
