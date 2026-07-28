//! 文件系统工具

use std::path::Path;

/// 原子写文件：先写同目录临时文件，再 rename 覆盖目标。
///
/// 直接 `fs::write` 在崩溃 / 掉电时会留下半截文件，下次读取解析失败即静默丢弃
/// 全部内容（对缓存状态文件表现为「缓存被重置」）。同目录 rename 保证同一文件
/// 系统，POSIX 下原子；Windows 上 `fs::rename` 覆盖已存在目标会失败，故失败后
/// 回退为直写，至少不比原来差。
///
/// 在 tokio 运行时内调用时用 `block_in_place` 让出，避免阻塞 worker 线程
/// （与 `token_manager::persist_credentials` 的处理一致）。
pub fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // 同目录临时文件：与目标同文件系统，rename 才可能原子。
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, data)?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            // Windows 上 rename 到已存在路径会报错；退化为直写。
            Err(_) => std::fs::write(path, data),
        }
    };

    let result = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(write)
    } else {
        write()
    };

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kiro-fs-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_content_and_leaves_no_tmp_residue() {
        let dir = tmp_dir("basic");
        let path = dir.join("state.json");
        write_file_atomic(&path, b"hello").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let residue: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(residue.is_empty(), "临时文件应已被 rename 消耗");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = tmp_dir("overwrite");
        let path = dir.join("state.json");
        write_file_atomic(&path, b"first").unwrap();
        write_file_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_parent_dirs() {
        let dir = tmp_dir("nested");
        let path = dir.join("a").join("b").join("state.json");
        write_file_atomic(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
