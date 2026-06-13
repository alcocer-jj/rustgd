#' View a data frame in a rustgd frames window.
#'
#' Writes the data frame to the frames channel as Arrow IPC and opens, or
#' reuses, the rustgd frames window to display it. Non-blocking: it returns at
#' once and the window appears on its own, the same way the plot and web
#' viewers behave. Each call adds a frame; later stages page through them as a
#' gallery.
#'
#' The frame is written uncompressed so the viewer needs no compression codec.
#' Requires the `arrow` package for `write_feather()`.
#'
#' @param df A data frame, or an object coercible to one.
#' @param title Optional label shown in the window; defaults to the expression
#'   passed in, for example `view(mtcars)` is labeled `mtcars`.
#' @return The path to the written Arrow file, invisibly.
#' @export
view <- function(df, title = NULL) {
  label <- if (is.null(title)) deparse(substitute(df))[1] else title
  if (!is.data.frame(df)) {
    df <- as.data.frame(df)
  }
  if (!requireNamespace("arrow", quietly = TRUE)) {
    stop(
      "view(): the 'arrow' package is required. Install it with ",
      "install.packages('arrow').",
      call. = FALSE
    )
  }

  frames <- .rustgd_frames_dir()
  dir.create(frames, recursive = TRUE, showWarnings = FALSE)
  .rustgd_frames_register_cleanup()

  # If no live window is showing this directory (it was closed, R quit, or the
  # viewer was killed), wipe any leftovers so the window reopens showing just
  # this frame rather than the previous session's.
  if (!.rustgd_frames_alive(frames)) {
    .rustgd_frames_reset(frames)
    .rustgd_state$frames_dir <- NULL
  }

  idx <- .rustgd_frames_next_index(frames)
  stem <- sprintf("frame-%04d", idx)
  arrow_name <- paste0(stem, ".arrow")
  arrow_path <- file.path(frames, arrow_name)

  # Atomic writes: temp name then rename, so the viewer never reads a partial
  # file. Write the data first, then the descriptor that points at it, so the
  # descriptor only appears once its Arrow file is complete.
  tmp_arrow <- paste0(arrow_path, ".tmp")
  arrow::write_feather(df, tmp_arrow, compression = "uncompressed")
  file.rename(tmp_arrow, arrow_path)

  lines <- c(
    "kind=frame",
    paste0("entry=", arrow_name),
    paste0("title=", label),
    paste0("full_rows=", nrow(df))
  )
  descriptor <- file.path(frames, paste0(stem, ".txt"))
  tmp_desc <- paste0(descriptor, ".tmp")
  writeLines(lines, tmp_desc)
  file.rename(tmp_desc, descriptor)

  .rustgd_frames_ensure(frames)
  invisible(arrow_path)
}

#' Internal: per-process frames directory, a sibling of the plots and widgets
#' directories. Uses the session temp directory so it works on every platform
#' (Windows has no /tmp); the binary receives this path as an argument.
.rustgd_frames_dir <- function() {
  file.path(tempdir(), paste0("rustgd-frames-", Sys.getpid()))
}

#' Internal: next descriptor number, one past the highest existing
#' `frame-NNNN.txt`.
#' @noRd
.rustgd_frames_next_index <- function(frames) {
  existing <- list.files(frames, pattern = "^frame-[0-9]+\\.txt$")
  nums <- suppressWarnings(
    as.integer(sub("^frame-([0-9]+)\\.txt$", "\\1", existing))
  )
  nums <- nums[!is.na(nums)]
  if (length(nums) > 0) max(nums) + 1L else 1L
}

#' Internal: launch the frames binary for this directory if it is not already
#' the one we have running. Liveness is decided up front in `view()` via
#' [.rustgd_frames_alive()], which clears `.rustgd_state$frames_dir` when the
#' window is gone, so here we simply launch whenever it is not our active
#' directory.
#' @noRd
.rustgd_frames_ensure <- function(frames) {
  if (isTRUE(.rustgd_state$frames_dir == frames)) {
    return(invisible(NULL))
  }
  # Clear the close marker and any stale pid before launching, so the liveness
  # check never reads a dead pid during the new viewer's startup.
  unlink(file.path(frames, "viewer_closed"))
  unlink(file.path(frames, "viewer.pid"))
  bin <- .rustgd_frames_bin()
  system2(bin, args = frames, wait = FALSE)
  .rustgd_state$frames_dir <- frames
  invisible(NULL)
}

#' Internal: whether a viewer window is genuinely live for this directory. True
#' only if we launched for it, no close marker is present, and the pid the
#' viewer recorded is still running. A force-killed viewer leaves no marker, so
#' a pid that no longer exists is how we detect it and relaunch.
#' @noRd
.rustgd_frames_alive <- function(frames) {
  if (!isTRUE(.rustgd_state$frames_dir == frames)) {
    return(FALSE)
  }
  if (file.exists(file.path(frames, "viewer_closed"))) {
    return(FALSE)
  }
  pidfile <- file.path(frames, "viewer.pid")
  if (!file.exists(pidfile)) {
    # Launched, but the viewer has not written its pid yet: assume starting.
    return(TRUE)
  }
  pid <- suppressWarnings(as.integer(readLines(pidfile, n = 1L, warn = FALSE)))
  if (length(pid) != 1L || is.na(pid)) {
    return(TRUE)
  }
  .rustgd_pid_alive(pid)
}

#' Internal: TRUE if a process with this pid is currently running. Uses
#' `kill -0` on Unix and `tasklist` on Windows, so the frames liveness check
#' works on every platform. The Windows branch is best-effort.
#' @noRd
.rustgd_pid_alive <- function(pid) {
  if (.Platform$OS.type == "windows") {
    out <- tryCatch(
      system2(
        "tasklist",
        c("/FI", shQuote(paste0("PID eq ", pid)), "/NH"),
        stdout = TRUE,
        stderr = FALSE
      ),
      error = function(e) character()
    )
    # The /FI filter returns the process line only when that pid exists, so a
    # line carrying the pid means it is running. Checking for the pid (rather
    # than the absence of a "no tasks" message) avoids locale dependence.
    any(grepl(paste0("(^|\\s)", pid, "(\\s|$)"), out))
  } else {
    # `kill -0` exits 0 when the process exists, nonzero when it does not.
    status <- suppressWarnings(
      system2("kill", c("-0", pid), stdout = FALSE, stderr = FALSE)
    )
    identical(as.integer(status), 0L)
  }
}

#' Internal: remove this directory's frame files, close marker, and pid file so
#' the next view() reopens the window fresh.
#' @noRd
.rustgd_frames_reset <- function(frames) {
  old <- list.files(
    frames,
    pattern = "^frame-[0-9]+\\.(arrow|txt)$",
    full.names = TRUE
  )
  unlink(old)
  unlink(file.path(frames, "viewer_closed"))
  unlink(file.path(frames, "viewer.pid"))
  invisible(NULL)
}

#' Internal: remove the frames directory when R exits.
.rustgd_frames_register_cleanup <- function() {
  if (isTRUE(.rustgd_state$frames_cleanup_registered)) {
    return(invisible(NULL))
  }
  reg.finalizer(
    .rustgd_state,
    function(e) {
      dir <- .rustgd_frames_dir()
      if (dir.exists(dir)) {
        unlink(dir, recursive = TRUE, force = TRUE)
      }
    },
    onexit = TRUE
  )
  .rustgd_state$frames_cleanup_registered <- TRUE
  invisible(NULL)
}

#' Internal: absolute path to the staged rustgd-frames binary.
.rustgd_frames_bin <- function() {
  exe <- if (.Platform$OS.type == "windows") "rustgd-frames.exe" else "rustgd-frames"
  bin <- system.file("bin", exe, package = "rustgd")
  if (!nzchar(bin)) {
    stop(
      "rustgd-frames binary not found. Build it with ",
      "`cargo build --release --bin rustgd-frames --features frames` ",
      "and stage it into inst/bin.",
      call. = FALSE
    )
  }
  bin
}
