//! The programs that play sound, from their audio sessions on the output
//! devices (what the Volume Mixer lists), and the volume one of them is
//! set to there.
//!
//! The process loopback of one program hears it after the volume of its
//! session: a session at volume `v` comes out `v` times as loud (measured
//! with `examples/app_loopback.rs`: 50 % is −6 dB, 10 % is −20 dB), and a
//! muted one silent. [`Volume::gain`] is the factor that undoes it. The
//! device's master volume does not reach the process loopback when the
//! device applies it in hardware, as it does on the development machine.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, eRender,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::core::{Interface, PCWSTR, PWSTR, Result};

use crate::win;

/// A program with an audio session.
#[derive(Clone, Debug)]
pub struct App {
    /// The full path of the executable: what identifies the program
    /// between runs (two copies of a browser may both be `vivaldi.exe`).
    pub path: String,
    /// The description in the executable (`Firefox`), or its file name;
    /// with the folder that tells it apart when two programs have the
    /// same name, `Vivaldi (VP)`.
    pub name: String,
    /// Whether it is playing now.
    pub active: bool,
}

/// The programs with an audio session on an output device, but this one
/// and the system sounds, by name.
pub fn apps() -> Vec<App> {
    // The window's thread has COM as an STA already, as a rule.
    let _com = win::com_init_mta();
    let own = unsafe { GetCurrentProcessId() };
    let mut by_path: HashMap<String, App> = HashMap::new();
    for session in sessions() {
        let Ok(pid) = (unsafe { session.GetProcessId() }) else { continue };
        if pid == 0 || pid == own || unsafe { session.IsSystemSoundsSession() }.0 == 0 {
            continue;
        }
        let Some(path) = image_path(pid) else { continue };
        let active = unsafe { session.GetState() }.is_ok_and(|s| s == AudioSessionStateActive);
        let key = path.to_lowercase();
        if let Some(app) = by_path.get_mut(&key) {
            app.active |= active;
            continue;
        }
        let name = description(&path).unwrap_or_else(|| stem(&path).to_owned());
        by_path.insert(key, App { path, name, active });
    }
    let mut apps: Vec<App> = by_path.into_values().collect();
    let mut same: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, app) in apps.iter().enumerate() {
        same.entry(app.name.to_lowercase()).or_default().push(i);
    }
    for group in same.values().filter(|g| g.len() > 1) {
        let paths: Vec<&str> = group.iter().map(|&i| apps[i].path.as_str()).collect();
        let folders = distinguish(&paths);
        for (&i, folder) in group.iter().zip(folders) {
            apps[i].name = format!("{} ({folder})", apps[i].name);
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// The first part of the paths that is not the same in all of them, for
/// each: the folder that tells programs of one name apart.
fn distinguish(paths: &[&str]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths.iter().map(|p| p.split('\\').collect()).collect();
    let shortest = parts.iter().map(Vec::len).min().unwrap_or(0);
    let i = (0..shortest).find(|&i| parts.iter().any(|p| !p[i].eq_ignore_ascii_case(parts[0][i]))).unwrap_or(0);
    parts.iter().map(|p| p.get(i).copied().unwrap_or_default().to_owned()).collect()
}

/// The executable's file name without `.exe`, from its path or its name:
/// how a program that is not running is named.
pub fn stem(program: &str) -> &str {
    let file = program.rsplit('\\').next().unwrap_or(program);
    file.len().checked_sub(4).filter(|&n| file[n..].eq_ignore_ascii_case(".exe")).map_or(file, |n| &file[..n])
}

/// The process to capture for a program, given by the full path of its
/// executable or by its file name: the topmost of its processes (a
/// browser plays from a child process of the same executable). When it
/// runs more than once, the copy that plays now is taken, then one that
/// has an audio session. `None` when it is not running.
pub fn find(program: &str) -> Option<u32> {
    let _com = win::com_init_mta();
    let file = program.rsplit('\\').next().unwrap_or(program);
    let by_path = program.contains('\\');
    let processes = processes();
    let parents: HashMap<u32, u32> = processes.iter().map(|p| (p.pid, p.parent)).collect();
    let ours: HashSet<u32> = processes
        .iter()
        .filter(|p| p.exe.eq_ignore_ascii_case(file))
        .filter(|p| !by_path || image_path(p.pid).is_some_and(|i| i.eq_ignore_ascii_case(program)))
        .map(|p| p.pid)
        .collect();
    let roots: Vec<u32> = processes.iter().filter(|p| ours.contains(&p.pid) && !ours.contains(&p.parent)).map(|p| p.pid).collect();
    let sessions: Vec<(u32, bool)> = sessions()
        .iter()
        .filter_map(|s| {
            let pid = unsafe { s.GetProcessId() }.ok()?;
            Some((pid, unsafe { s.GetState() }.is_ok_and(|state| state == AudioSessionStateActive)))
        })
        .collect();
    let plays = |root: u32, now: bool| sessions.iter().any(|&(pid, active)| (active || !now) && in_tree(pid, root, &parents));
    if roots.len() > 1 {
        log::debug!("audio: {program} runs {} times: {roots:?}", roots.len());
    }
    roots
        .iter()
        .copied()
        .find(|&r| plays(r, true))
        .or_else(|| roots.iter().copied().find(|&r| plays(r, false)))
        .or(roots.first().copied())
}

/// The volume of a program's sessions, followed while it is recorded.
pub struct Volume {
    root: u32,
    sessions: Vec<ISimpleAudioVolume>,
    listed: Option<Instant>,
}

// The session objects are free-threaded: made on one recording thread,
// read on the audio thread.
unsafe impl Send for Volume {}

/// The quietest volume undone; below it the sound is mostly lost.
const MIN_VOLUME: f32 = 0.01;

impl Volume {
    /// The sessions of the process tree from `root`.
    pub fn new(root: u32) -> Volume {
        Volume { root, sessions: Vec::new(), listed: None }
    }

    /// The factor that brings the captured sound to full volume: `1/v`
    /// for the loudest of the program's sessions that is not muted. The
    /// sessions are listed again every second, as the program opens new
    /// ones or new child processes.
    pub fn gain(&mut self) -> f32 {
        if self.listed.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            self.list();
        }
        let loudest = self
            .sessions
            .iter()
            .filter(|s| !unsafe { s.GetMute() }.is_ok_and(|m| m.as_bool()))
            .filter_map(|s| unsafe { s.GetMasterVolume() }.ok())
            .fold(None, |max: Option<f32>, v| Some(max.map_or(v, |m| m.max(v))));
        match loudest {
            Some(v) => gain_for(v),
            None => 1.0,
        }
    }

    fn list(&mut self) {
        self.listed = Some(Instant::now());
        let parents: HashMap<u32, u32> = processes().iter().map(|p| (p.pid, p.parent)).collect();
        let before = self.sessions.len();
        self.sessions = sessions()
            .into_iter()
            .filter(|s| unsafe { s.GetProcessId() }.is_ok_and(|pid| in_tree(pid, self.root, &parents)))
            .filter_map(|s| s.cast::<ISimpleAudioVolume>().ok())
            .collect();
        if self.sessions.len() != before {
            log::debug!("audio: {} sessions of process {}", self.sessions.len(), self.root);
        }
    }
}

/// `1/v`, at most that of [`MIN_VOLUME`].
fn gain_for(volume: f32) -> f32 {
    1.0 / volume.clamp(MIN_VOLUME, 1.0)
}

/// The audio sessions on every active output device.
fn sessions() -> Vec<IAudioSessionControl2> {
    let result = (|| -> Result<Vec<IAudioSessionControl2>> {
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
        let mut sessions = Vec::new();
        for i in 0..unsafe { devices.GetCount()? } {
            let Ok(device) = (unsafe { devices.Item(i) }) else { continue };
            let Ok(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }) else { continue };
            let Ok(list) = (unsafe { manager.GetSessionEnumerator() }) else { continue };
            for j in 0..unsafe { list.GetCount() }.unwrap_or(0) {
                if let Ok(session) = unsafe { list.GetSession(j) }.and_then(|s| s.cast::<IAudioSessionControl2>()) {
                    sessions.push(session);
                }
            }
        }
        Ok(sessions)
    })();
    result.unwrap_or_else(|e| {
        log::warn!("audio sessions not listed: {}", win::describe(&e));
        Vec::new()
    })
}

struct Process {
    pid: u32,
    parent: u32,
    exe: String,
}

/// The running processes.
fn processes() -> Vec<Process> {
    let mut list = Vec::new();
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return list };
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
    while next.is_ok() {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        list.push(Process {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
        });
        next = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    list
}

/// Whether `pid` is `root` or descends from it. A parent may have exited
/// and its id been reused; the walk is bounded.
fn in_tree(mut pid: u32, root: u32, parents: &HashMap<u32, u32>) -> bool {
    for _ in 0..64 {
        if pid == root {
            return true;
        }
        match parents.get(&pid) {
            Some(&parent) if parent != 0 && parent != pid => pid = parent,
            _ => return false,
        }
    }
    false
}

/// The full path of a process's executable.
fn image_path(pid: u32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0u16; MAX_PATH as usize * 4];
    let mut len = buffer.len() as u32;
    let result = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) };
    unsafe {
        let _ = CloseHandle(process);
    }
    result.ok()?;
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// The file description in an executable's version resource, in its
/// first language.
fn description(path: &str) -> Option<String> {
    let wide = win::wide(path);
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast()) }.ok()?;
    let query = |key: &str| -> Option<(*const u8, usize)> {
        let key = win::wide(key);
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        let found = unsafe { VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len) };
        (found.as_bool() && !ptr.is_null() && len > 0).then_some((ptr as *const u8, len as usize))
    };
    let (ptr, len) = query("\\VarFileInfo\\Translation")?;
    if len < 4 {
        return None;
    }
    let (lang, codepage) = unsafe { (*(ptr as *const u16), *(ptr as *const u16).add(1)) };
    let (ptr, len) = query(&format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"))?;
    let chars = unsafe { std::slice::from_raw_parts(ptr as *const u16, len) };
    let text = String::from_utf16_lossy(&chars[..chars.iter().position(|&c| c == 0).unwrap_or(len)]);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_undoes_the_volume() {
        assert_eq!(gain_for(1.0), 1.0);
        assert_eq!(gain_for(0.5), 2.0);
        assert!((gain_for(0.1) - 10.0).abs() < 1e-4);
        assert!((gain_for(0.0) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn stem_drops_the_folder_and_the_extension() {
        assert_eq!(stem("firefox.exe"), "firefox");
        assert_eq!(stem("Spotify.EXE"), "Spotify");
        assert_eq!(stem("C:\\Program Files\\Mozilla Firefox\\firefox.exe"), "firefox");
        assert_eq!(stem("exe"), "exe");
    }

    #[test]
    fn programs_of_one_name_are_told_apart_by_folder() {
        let paths = [
            "C:\\Users\\A\\AppData\\Local\\Vivaldi\\Application\\vivaldi.exe",
            "C:\\Users\\A\\AppData\\Local\\VP\\Application\\vivaldi.exe",
        ];
        assert_eq!(distinguish(&paths), vec!["Vivaldi", "VP"]);
        let paths = ["D:\\Tools\\player.exe", "D:\\Tools\\player2.exe"];
        assert_eq!(distinguish(&paths), vec!["player.exe", "player2.exe"]);
    }

    #[test]
    fn tree_follows_parents() {
        let parents = HashMap::from([(10, 1), (11, 10), (12, 11), (20, 1)]);
        assert!(in_tree(12, 10, &parents));
        assert!(in_tree(10, 10, &parents));
        assert!(!in_tree(20, 10, &parents));
        assert!(!in_tree(99, 10, &parents));
    }
}
