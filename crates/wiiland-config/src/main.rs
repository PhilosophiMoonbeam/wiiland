mod config_task;
mod live;
mod model;
mod process;
mod theme;
mod ui;

use std::io::Write;

use eframe::egui;

use model::{
    ApplyCompletion, Completion, ConfigFailure, ConfigFailureStage, ConfigModel, ConfigValue,
    OUTPUT_BLOCK_LIMIT, TransactionKind,
};
use ui::ControlCenter;

const APPLICATION_ID: &str = "io.github.philosophimoonbeam.wiiland-config";

fn main() -> eframe::Result {
    if std::env::var_os("WIILAND_CONFIG_SMOKE_TEST").as_deref() == Some(std::ffi::OsStr::new("1")) {
        if let Err(error) = write_smoke_report() {
            eprintln!("wiiland-config smoke: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let app = ControlCenter::initialize(ConfigModel::default());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APPLICATION_ID)
            .with_title("WiiLand Control Center")
            .with_icon(
                eframe::icon_data::from_png_bytes(theme::ICON).expect("embedded WiiLand icon"),
            )
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([760.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "WiiLand Control Center",
        options,
        Box::new(move |creation_context| {
            theme::install(&creation_context.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}

fn write_smoke_report() -> std::io::Result<()> {
    let default_path = ConfigModel::default_path();
    let default_path_absolute = default_path.as_ref().is_some_and(|path| path.is_absolute());
    let explicit_path =
        std::env::temp_dir().join(format!("wiiland-config-smoke-{}.conf", std::process::id()));
    let mut model = ConfigModel::new(explicit_path.clone());
    model.config.profile = wiiland_core::Profile::DESKTOP;
    model.mark_dirty();

    // A load completion captured before the edit is rejected by revision, while
    // the transaction itself is released so controls recover after failures.
    let load = model
        .begin(TransactionKind::Load, Vec::new())
        .expect("smoke load transaction");
    model.mark_dirty();
    let stale_load = Completion::new(
        &load,
        Ok(ConfigValue::Loaded(
            model::parse_config_bytes(b"profile=gamepad\npointer-speed=99\n").unwrap(),
        )),
    );
    let load_transaction_safe =
        model.finish(&stale_load) == ApplyCompletion::Stale && model.transaction.is_none();

    // Saves carry their rendered bytes in the transaction. The form can change
    // while validation runs, but the validated snapshot is what gets persisted.
    let saved_snapshot = model.render();
    let save = model
        .begin(TransactionKind::Save, saved_snapshot.clone())
        .expect("smoke save transaction");
    model.config.pointer_speed = 18;
    model.mark_dirty();
    let save_task = config_task::ConfigTask::spawn(save, "/bin/true".to_owned(), None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let save_completion = loop {
        match save_task.try_recv() {
            Ok(config_task::ConfigEvent::Finished(completion)) => break completion,
            Ok(_) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => return Err(std::io::Error::other(format!("smoke save worker: {error}"))),
        }
    };
    let save_transaction_safe = model.finish(&save_completion) == ApplyCompletion::Stale
        && std::fs::read(&explicit_path)
            .map(|bytes| bytes == saved_snapshot)
            .unwrap_or(false);

    let error = model
        .begin(TransactionKind::Load, Vec::new())
        .expect("smoke error transaction");
    let error_completion = Completion::new(
        &error,
        Err(ConfigFailure {
            stage: ConfigFailureStage::Load,
            message: "delayed fake load failure".to_owned(),
        }),
    );
    let error_recovered =
        model.finish(&error_completion) == ApplyCompletion::Failed && model.transaction.is_none();

    model.config.aim_accel_zero = Some(model::calibration_default());
    model.config.aim_accel_zero.as_mut().unwrap().x = 11;
    model.config.aim_accel_zero.as_mut().unwrap().y = 12;
    model.config.aim_accel_zero.as_mut().unwrap().z = 13;
    let rendered = model.render();
    let calibration_isolated = rendered
        .windows(b"aim-accel-zero-x=11\n".len())
        .any(|window| window == b"aim-accel-zero-x=11\n")
        && !rendered
            .windows(b"aim-motion-plus-bias-".len())
            .any(|window| window == b"aim-motion-plus-bias-");
    let mut output = model::OutputBuffer::new();
    for _ in 0..(OUTPUT_BLOCK_LIMIT + 2) {
        output.append("line\n");
    }
    let output_bounded = output.block_count() == OUTPUT_BLOCK_LIMIT;

    let report = format!(
        "eframe.platform={}\nservice.restart.explicit-config=disabled\ncalibration.partial-source={}\nconfig.choice-values=canonical\nconfig.compact-layout=responsive\nconfig.default-path={}\nconfig.unsaved-state=tracked\nconfig.transaction.load={}\nconfig.transaction.save={}\nconfig.transaction.error={}\noutput.actions=available\noutput.buffer={}\nvalidation.controls=coordinated\nvalidation.form=visible\n",
        ControlCenter::backend_name().to_ascii_lowercase(),
        if calibration_isolated {
            "isolated"
        } else {
            "coupled"
        },
        if default_path_absolute {
            "absolute"
        } else {
            "invalid"
        },
        if load_transaction_safe {
            "revision-safe"
        } else {
            "stale"
        },
        if save_transaction_safe {
            "revision-safe"
        } else {
            "stale"
        },
        if error_recovered {
            "recovered"
        } else {
            "stuck"
        },
        if output_bounded {
            "bounded"
        } else {
            "unbounded"
        },
    );
    let result = std::io::stdout().write_all(report.as_bytes());
    let _ = std::fs::remove_file(explicit_path);
    result
}
