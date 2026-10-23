# Residuals report
1. settings_window.rs help text reworded to say listed programs, relative paths and commands naming one still ask; unlisted programs can be approved. Shots regenerated (target/ui-shots, untracked).
2. bash.rs busy/timeout/thread-failure paths now use `Some(Instant::now())`; model_stage.rs deadline check confirmed to run only before an uncached call. New test `bash_busy_classifier_still_uses_cached_outcome`.
3. `bash_busy_classifier_is_not_stacked`: timeout 2000ms, asserts elapsed < 1000ms.
Verified: fmt, clippy (default and ai-testing), workspace tests with ai-testing all pass.
