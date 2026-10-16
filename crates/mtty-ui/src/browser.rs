//! Browser verification evidence owned by a local project/task.
use crate::{project, task_review, tasks};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Context {
    pub cwd: PathBuf,
    pub task: Option<tasks::Task>,
    pub revision: u64,
    pub directory: PathBuf,
}
#[derive(Clone, Debug)]
pub struct Plan {
    pub context: Context,
    pub url: String,
    pub screenshot: PathBuf,
    pub notes: String,
    pub command: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub url: String,
    pub screenshot: String,
    pub notes: String,
    pub valid: bool,
    pub width: u32,
    pub height: u32,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Report {
    pub evidence: Evidence,
    pub exit: Option<i32>,
    pub output: String,
}

pub fn validate_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let prefix = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return Err("enter an http:// or https:// service URL".into());
    };
    if url[prefix..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .is_empty()
        || url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err("invalid browser verification URL".into());
    }
    Ok(url.to_owned())
}
pub fn context(cwd: &Path) -> Result<Context, String> {
    let task = task_review::task_at(cwd).ok();
    let cwd = task
        .as_ref()
        .map(|task| task.path.clone())
        .or_else(|| project::load(cwd).ok().map(|source| source.root))
        .unwrap_or_else(|| cwd.to_path_buf());
    let revision = task
        .as_ref()
        .map(task_review::snapshot)
        .transpose()?
        .map_or(0, |snapshot| snapshot.fingerprint);
    let directory = if let Some(task) = &task {
        let out = mtty_platform::background_command("git")
            .arg("-C")
            .arg(&task.path)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "mtty-browser",
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !out.status.success() {
            return Err("cannot locate browser evidence directory".into());
        }
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    } else {
        mtty_config::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("browser-evidence")
    };
    Ok(Context {
        cwd,
        task,
        revision,
        directory,
    })
}
pub fn plan(context: Context, url: &str, command: &str, notes: &str) -> Result<Plan, String> {
    let url = validate_url(url)?;
    if command.trim().is_empty() {
        return Err("enter an automation command or configure browser-command".into());
    }
    std::fs::create_dir_all(&context.directory).map_err(|error| error.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let screenshot = context
        .directory
        .join(format!("capture-{}-{nonce}.png", std::process::id()));
    let variables = std::collections::BTreeMap::from([
        ("project".into(), context.cwd.to_string_lossy().into_owned()),
        ("url".into(), url.clone()),
        (
            "screenshot".into(),
            screenshot.to_string_lossy().into_owned(),
        ),
    ]);
    let task = project::Task {
        name: "Browser verification".into(),
        command: command.into(),
        cwd: Some("${project}".into()),
        host: None,
    };
    let resolved = project::resolve(&context.cwd, &task, &variables)?;
    Ok(Plan {
        context,
        url,
        screenshot,
        notes: notes.chars().take(8192).collect(),
        command: resolved.command,
    })
}
fn inspect_png(path: &Path) -> Result<(u32, u32), String> {
    if std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len()
        > 32 * 1024 * 1024
    {
        return Err("screenshot exceeds 32 MiB".into());
    }
    let mut decoder =
        png::Decoder::new(std::fs::File::open(path).map_err(|error| error.to_string())?);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let width = reader.info().width;
    let height = reader.info().height;
    let size = reader.output_buffer_size();
    if width == 0 || height == 0 || size > 256 * 1024 * 1024 {
        return Err("screenshot dimensions exceed the preview budget".into());
    }
    let mut pixels = vec![0; size];
    reader
        .next_frame(&mut pixels)
        .map_err(|error| error.to_string())?;
    Ok((width, height))
}
pub fn finish(plan: Plan, exit: Option<i32>, output: String) -> Result<Report, String> {
    let image = inspect_png(&plan.screenshot);
    let (width, height) = image.as_ref().copied().unwrap_or((0, 0));
    let evidence = Evidence {
        url: plan.url,
        screenshot: plan.screenshot.to_string_lossy().into_owned(),
        notes: plan.notes,
        valid: image.is_ok(),
        width,
        height,
        error: image.err(),
    };
    let output = format!(
        "URL: {}\nScreenshot: {}\nNotes: {}\nArtifact: {}\n\n{}",
        evidence.url,
        evidence.screenshot,
        evidence.notes,
        evidence.error.as_deref().unwrap_or("valid PNG"),
        output
    );
    if let Some(task) = &plan.context.task {
        task_review::record_check(
            task,
            task_review::Verification {
                command: plan.command,
                exit,
                output: output.clone(),
                revision: plan.context.revision,
                finished_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_secs()),
                browser: Some(evidence.clone()),
            },
        )?;
    }
    Ok(Report {
        evidence,
        exit,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_service_urls_and_requires_a_real_image_for_a_pass() {
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("http://").is_err());
        assert_eq!(
            validate_url(" http://localhost:3000/a ").unwrap(),
            "http://localhost:3000/a"
        );
        let directory = std::env::temp_dir().join(format!(
            "mtty-browser-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let context = Context {
            cwd: directory.clone(),
            task: None,
            revision: 0,
            directory: directory.clone(),
        };
        let plan = plan(
            context,
            "http://localhost:3000",
            "echo ${url} ${screenshot}",
            "notes",
        )
        .unwrap();
        let report = finish(plan, Some(0), "tool returned zero".into()).unwrap();
        assert!(!report.evidence.valid);
        assert!(report.evidence.error.is_some());
        let path = directory.join("valid.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, 2, 2);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0; 12]).unwrap();
        drop(writer);
        assert_eq!(inspect_png(&path).unwrap(), (2, 2));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
