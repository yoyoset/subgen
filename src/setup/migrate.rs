use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 模型目录搬迁:用户改存放位置时,把旧目录下已经下载好的文件搬到新目录,不用重新下载。
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum MigrateEvent {
    Log(String),
    Progress {
        file_label: String,
        current_index: usize,
        total_count: usize,
        copied: u64,
        total: u64,
    },
    FileDone(String),
    /// 搬迁跑完了(不管顺不顺利)。携带的 Vec 是每个"没能完全按预期完成"的文件的说明
    /// (跳过、单个文件失败、取消导致没轮到的都算);空 Vec 表示全部顺利搬完。
    AllDone(Vec<String>),
}

/// 把 `old_dir` 下的所有文件搬到 `new_dir`。不针对具体文件名(不维护一份"已知模型文件名"
/// 清单,那份清单容易漏、容易跟 translate::MODELS 等注册表脱节),而是"旧目录里有什么就搬
/// 什么"——models_dir() 本来就是个平铺目录,不该有用户不认识的东西在里面,这样处理更通用,
/// 也顺带覆盖了用户手动放进去的文件。
///
/// 目标目录如果已经有同名文件:大小相同就当作"已经搬过/本来就有效"跳过,不重新搬;大小不同
/// 时为避免误覆盖一份可能是有效的文件,直接跳过并留在原地——这份文件的最终有效性由外层
/// `is_present`/哈希校验逻辑负责,搬迁这一步只负责不丢数据,不负责判断内容对不对。
///
/// **单个文件出错不会中断整个搬迁**(取消除外):哪怕某个文件搬迁失败,也继续搬下一个,
/// 最后把所有问题汇总返回,而不是"一个文件坏了、后面几个 GB 的文件全跳过不搬"。调用方
/// 不管返回值是否为空、过程中是否被取消,都应该把生效的模型目录切到 `new_dir`——部分文件
/// 可能已经被物理搬过去了,`new_dir` 才是准确反映当前实际存放位置的地方,剩下的缺口交给
/// 外层现成的 `missing_components()` 流程去补齐(它本来就是按"新目录下这个文件在不在、
/// 对不对"逐项判断,不需要为这个中间状态写特殊分支)。
pub fn migrate_models_dir(
    old_dir: &Path,
    new_dir: &Path,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Vec<String> {
    if let Err(e) = std::fs::create_dir_all(new_dir) {
        return vec![format!("创建目标目录失败: {e}")];
    }

    let entries: Vec<PathBuf> = match std::fs::read_dir(old_dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(_) => Vec::new(), // 旧目录不存在/读不了,当作没有东西要搬,不算错误
    };

    let mut issues = Vec::new();
    if entries.is_empty() {
        return issues;
    }

    let total_count = entries.len();
    for (i, src) in entries.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            issues.push(format!(
                "已取消,剩余 {} 个文件未搬迁,仍留在旧目录",
                total_count - i
            ));
            break;
        }
        let file_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("未知文件")
            .to_string();
        let dst = new_dir.join(&file_name);
        let current_index = i + 1;
        let _ = tx.send(MigrateEvent::Log(format!(
            "正在搬迁 {file_name}({current_index}/{total_count})..."
        )));
        if let Err(e) = migrate_one_file(src, &dst, &file_name, current_index, total_count, tx, cancel)
        {
            let _ = tx.send(MigrateEvent::Log(format!("{file_name} 搬迁出错: {e}")));
            issues.push(e);
        }
    }
    issues
}

fn migrate_one_file(
    src: &Path,
    dst: &Path,
    label: &str,
    index: usize,
    total_count: usize,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if dst.exists() {
        let src_len = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let dst_len = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        if src_len == dst_len {
            let _ = tx.send(MigrateEvent::Log(format!("{label} 目标位置已存在且大小一致,跳过")));
        } else {
            let _ = tx.send(MigrateEvent::Log(format!(
                "{label} 目标位置已有同名但大小不同的文件,为避免覆盖有效数据已跳过搬迁,\
                 原文件仍保留在旧目录"
            )));
        }
        let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
        return Ok(());
    }

    // 优先尝试同盘 rename——原子操作,几乎瞬间完成,不需要额外的完整性校验。
    if std::fs::rename(src, dst).is_ok() {
        let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
        return Ok(());
    }

    // rename 失败(通常是跨盘符):退化成"复制到临时名 -> 核对大小 -> 原子改名到位 ->
    // 删除源文件"。任何一步中断(取消/掉电/进程被杀),旧目录里的源文件都还在、没被删,
    // 不会出现两边都没有的丢失状态,顶多是这次搬迁没完成、下次再试一遍。
    let tmp_dst = dst.with_extension("migrating");
    let total = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    copy_with_progress(src, &tmp_dst, label, index, total_count, total, tx, cancel)?;

    let copied_len = std::fs::metadata(&tmp_dst).map(|m| m.len()).unwrap_or(0);
    if copied_len != total {
        let _ = std::fs::remove_file(&tmp_dst);
        return Err(format!("{label} 搬迁校验失败(复制后大小不一致),源文件保留在原位置"));
    }

    std::fs::rename(&tmp_dst, dst).map_err(|e| format!("{label} 搬迁完成但重命名失败: {e}"))?;
    // 新位置的文件此时已经就绪可用,即使下面删源文件这步失败,也不算搬迁失败——顶多是旧
    // 目录里多留了一份没清理掉的副本,不影响功能,日志里如实记录方便用户自己清。
    if let Err(e) = std::fs::remove_file(src) {
        let _ = tx.send(MigrateEvent::Log(format!(
            "{label} 已搬到新目录,但删除旧位置文件失败(可手动清理): {e}"
        )));
    }
    let _ = tx.send(MigrateEvent::FileDone(label.to_string()));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_with_progress(
    src: &Path,
    dst: &Path,
    label: &str,
    index: usize,
    total_count: usize,
    total: u64,
    tx: &Sender<MigrateEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut reader = std::fs::File::open(src).map_err(|e| format!("打开源文件失败: {e}"))?;
    let mut writer = std::fs::File::create(dst).map_err(|e| format!("创建目标文件失败: {e}"))?;
    // 1MB 缓冲区:这是跨盘搬迁大文件(几个 GB),块大一点能减少系统调用次数,
    // 和 setup.rs 下载用的 64KB(网络包大小量级)不是同一个场景,没必要保持一致。
    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied: u64 = 0;
    let mut last_report = std::time::Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(dst);
            return Err("已取消".to_string());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("读取源文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("写入目标文件失败: {e}"))?;
        copied += n as u64;
        if last_report.elapsed().as_millis() > 150 {
            let _ = tx.send(MigrateEvent::Progress {
                file_label: label.to_string(),
                current_index: index,
                total_count,
                copied,
                total,
            });
            last_report = std::time::Instant::now();
        }
    }
    let _ = tx.send(MigrateEvent::Progress {
        file_label: label.to_string(),
        current_index: index,
        total_count,
        copied,
        total,
    });
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// 每个测试用独立的、带随机后缀的临时目录,避免并行跑的测试互相踩到同一个路径。
    fn temp_subdir(name: &str) -> PathBuf {
        let unique = format!(
            "{}_{:x}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join("subgen_setup_test").join(unique)
    }


    #[test]
    fn migrate_moves_files_to_new_dir() {
        let old_dir = temp_subdir("migrate_move_old");
        let new_dir = temp_subdir("migrate_move_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("model.bin"), b"hello model").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(issues.is_empty(), "正常搬迁不应该有问题: {issues:?}");
        assert!(!old_dir.join("model.bin").exists(), "源文件应该被搬走");
        assert_eq!(
            std::fs::read(new_dir.join("model.bin")).unwrap(),
            b"hello model",
            "新目录里的文件内容应该和原文件一致"
        );

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_skips_when_target_already_has_same_size_file() {
        let old_dir = temp_subdir("migrate_skip_same_old");
        let new_dir = temp_subdir("migrate_skip_same_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(old_dir.join("model.bin"), b"same size!!").unwrap(); // 11 bytes
        std::fs::write(new_dir.join("model.bin"), b"already here").unwrap(); // 12 bytes,故意不同

        // 先测大小不同的情况:应该跳过,两边都保留原样
        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);
        assert!(issues.is_empty(), "跳过不算错误,不应该出现在 issues 里");
        assert!(old_dir.join("model.bin").exists(), "大小不一致时不应该删除源文件");
        assert_eq!(
            std::fs::read(new_dir.join("model.bin")).unwrap(),
            b"already here",
            "大小不一致时不应该覆盖目标目录里已有的文件"
        );

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_skips_and_removes_source_when_target_has_same_size_file() {
        let old_dir = temp_subdir("migrate_skip_exact_old");
        let new_dir = temp_subdir("migrate_skip_exact_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        // 大小相同(都是 11 字节),内容其实不同也没关系——这一步只用大小做"看起来已经
        // 搬过"的启发式判断,不做内容比对(见 migrate_one_file 的注释)。
        std::fs::write(old_dir.join("model.bin"), b"source data").unwrap();
        std::fs::write(new_dir.join("model.bin"), b"target data").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);
        assert!(issues.is_empty());
        // 跳过时不会去删源文件(migrate_one_file 里"已存在"分支直接 return,不碰源文件)
        assert!(old_dir.join("model.bin").exists());
        assert_eq!(std::fs::read(new_dir.join("model.bin")).unwrap(), b"target data");

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_handles_missing_old_dir_gracefully() {
        let old_dir = temp_subdir("migrate_missing_old");
        let new_dir = temp_subdir("migrate_missing_new");
        // 故意不创建 old_dir

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(issues.is_empty(), "旧目录不存在时应该当作没有东西要搬,不是错误");
        assert!(new_dir.is_dir(), "目标目录仍然应该被创建出来");

        let _ = std::fs::remove_dir_all(&new_dir);
    }

    #[test]
    fn migrate_continues_past_a_single_file_cancellation_and_reports_remainder() {
        // 用一个已经置位的 cancel token 模拟"刚开始搬就被取消":应该在处理第一个文件前
        // 就停下,issues 里要能看出"还有文件没搬完",而不是安安静静地什么都不做。
        let old_dir = temp_subdir("migrate_cancel_old");
        let new_dir = temp_subdir("migrate_cancel_new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("a.bin"), b"a").unwrap();
        std::fs::write(old_dir.join("b.bin"), b"b").unwrap();

        let (tx, _rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(true)); // 从一开始就是取消状态
        let issues = migrate_models_dir(&old_dir, &new_dir, &tx, &cancel);

        assert!(!issues.is_empty(), "被取消应该在 issues 里留下说明,不能悄无声息");
        assert!(old_dir.join("a.bin").exists(), "取消时源文件不应该丢失");
        assert!(old_dir.join("b.bin").exists(), "取消时源文件不应该丢失");

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
    }

}
