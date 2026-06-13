#' Start the rustgd graphics device.
#'
#' Launches a native viewer window. When the viewer is resized, all
#' plots in the session history reflow automatically during R's idle
#' time, with no need to press Enter or run any command. Each plot's
#' recorded graphics display list (captured via grDevices::recordPlot
#' immediately after the plot was drawn) is replayed onto the
#' resized device via grDevices::replayPlot, which lets the
#' underlying plotting package (ggplot2, tmap, base R, lattice, etc.)
#' lay itself out fresh at the new dimensions without re-executing
#' any of the user's R code. If recording or replay fails for any
#' reason, the captured top-level expression is re-evaluated as a
#' fallback.
#'
#' Closing the viewer window propagates back to R: the next idle-time
#' poll detects the close, calls dev.off() on the rustgd device, and
#' the Rust-side close() callback removes the session directory. After
#' closing, calling rustgd() again opens a fresh window with no stray
#' state.
#'
#' @importFrom grDevices dev.control dev.cur dev.flush dev.list dev.off
#'   deviceIsInteractive recordPlot replayPlot
#' @export
rustgd <- function() {
  # If a previously tracked rustgd device is still open, close it
  # first so we start cleanly.
  prev_dev <- .rustgd_state$device_num
  if (!is.null(prev_dev) && prev_dev %in% dev.list()) {
    tryCatch(dev.off(prev_dev), error = function(e) NULL)
  }

  # Remove any stale resize-capture callback before re-registering.
  callbacks <- getTaskCallbackNames()
  if ("rustgd_resize_capture" %in% callbacks) {
    removeTaskCallback("rustgd_resize_capture")
  }

  # Register "rustgd" as an interactive device name. R consults this
  # list to decide things like whether dev.interactive() returns TRUE
  # and whether to set up devAskNewPage. Idempotent and safe to call
  # multiple times.
  tryCatch(
    grDevices::deviceIsInteractive("rustgd"),
    error = function(e) NULL
  )

  .rustgd_state$plot_history <- list()
  .rustgd_state$active <- TRUE
  .rustgd_state$session_dir <- file.path(tempdir(), paste0("rustgd-", Sys.getpid()))

  rustgd_activate(.rustgd_state$session_dir)
  .rustgd_state$device_num <- dev.cur()

  # Enable the graphics engine display list on this device. R's
  # default for devices that don't self-identify as screen devices
  # is to *inhibit* the display list (since "print" devices like
  # pdf() or svg() typically have no need to record). Without an
  # enabled display list, recordPlot() returns an essentially empty
  # object and replayPlot() does nothing visible, which breaks the
  # primary resize-replay path. Calling dev.control("enable") here,
  # immediately after the device is created, flips this on.
  tryCatch(
    grDevices::dev.control("enable"),
    error = function(e) NULL
  )

  # Task callback captures plot expressions after each user command.
  addTaskCallback(
    function(expr, value, ok, visible) {
      tryCatch(rustgd_task_callback(expr), error = function(e) NULL)
      TRUE
    },
    name = "rustgd_resize_capture"
  )

  # Schedule periodic resize check via the `later` package. The callback
  # fires during R's idle time and re-schedules itself.
  if (!requireNamespace("later", quietly = TRUE)) {
    warning("Package 'later' is required for automatic resize reflow. Install it with install.packages('later').")
    return(invisible(NULL))
  }
  later::later(rustgd_poll_resize, delay = 0.005)

  invisible(NULL)
}

# Package-level state.
.rustgd_state <- new.env(parent = emptyenv())
# plot_history is a list of plot entries, one per logical plot. Each
# entry is a named list with two fields:
#   - $recorded: the result of grDevices::recordPlot() captured
#     immediately after the plot was drawn. This is the primary
#     re-render mechanism on resize.
#   - $exprs: the captured top-level expressions that produced the
#     plot (one for a single-expression plot like a ggplot, multiple
#     for a layered base plot like plot() + lines() + points()).
#     Used as a fallback if recordPlot returned NULL or replayPlot
#     fails at resize time.
.rustgd_state$plot_history <- list()
.rustgd_state$active <- FALSE
.rustgd_state$device_num <- NULL
.rustgd_state$session_dir <- NULL
.rustgd_state$prev_device <- NULL

#' Internal: tear down the rustgd session. Removes the task callback,
#' clears state, and optionally closes the R device. Idempotent;
#' safe to call from poll handlers that may race with manual dev.off().
#' @noRd
rustgd_shutdown <- function(close_device = TRUE) {
  .rustgd_state$active <- FALSE

  if (close_device) {
    dev_num <- .rustgd_state$device_num
    if (!is.null(dev_num) && dev_num %in% dev.list()) {
      tryCatch(dev.off(dev_num), error = function(e) NULL)
    }
  }

  callbacks <- getTaskCallbackNames()
  if ("rustgd_resize_capture" %in% callbacks) {
    tryCatch(removeTaskCallback("rustgd_resize_capture"), error = function(e) NULL)
  }

  .rustgd_state$plot_history <- list()
  .rustgd_state$device_num <- NULL
  invisible(NULL)
}

#' Internal: scheduled poll for resize signals and viewer-close events.
#' Reschedules itself while the device is active.
rustgd_poll_resize <- function() {
  if (!isTRUE(.rustgd_state$active)) {
    return(invisible(NULL))
  }

  # If the device was closed externally (e.g. user called dev.off()
  # manually, or graphics.off()), stop polling.
  dev_num <- .rustgd_state$device_num
  if (is.null(dev_num) || !(dev_num %in% dev.list())) {
    rustgd_shutdown(close_device = FALSE)
    return(invisible(NULL))
  }

  # If the viewer window was closed, the viewer drops a marker file
  # into the session dir. Detect it, close the device (which fires
  # the Rust close() callback and removes the session dir), and stop
  # polling. The next rustgd() call gets a clean slate.
  session_dir <- .rustgd_state$session_dir
  if (!is.null(session_dir)) {
    closed_marker <- file.path(session_dir, "viewer_closed")
    if (file.exists(closed_marker)) {
      rustgd_shutdown(close_device = TRUE)
      return(invisible(NULL))
    }

    # Clear-all signal: wipe history and all SVG files.
    clear_all_marker <- file.path(session_dir, "clear_all.txt")
    if (file.exists(clear_all_marker)) {
      tryCatch(rustgd_handle_clear_all(), error = function(e) NULL)
    }

    # Clear-active signal: remove the specified plot from history
    # and renumber remaining files so the gallery stays contiguous.
    clear_plot_marker <- file.path(session_dir, "clear_plot.txt")
    if (file.exists(clear_plot_marker)) {
      tryCatch(rustgd_handle_clear_plot(), error = function(e) NULL)
    }
  }

  tryCatch(rustgd_input_handler(), error = function(e) NULL)
  later::later(rustgd_poll_resize, delay = 0.005)
  invisible(NULL)
}

#' Internal: process a pending resize if one exists. Reads resize.txt,
#' updates the device dimensions, and re-evaluates every plot in the
#' session history at the new size. Each plot is directed to its own
#' plot-NNNN.svg file via rustgd_set_current_page().
rustgd_input_handler <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) {
    return(invisible(NULL))
  }
  signal_file <- file.path(session_dir, "resize.txt")
  if (!file.exists(signal_file)) {
    return(invisible(NULL))
  }

  content <- tryCatch(
    readLines(signal_file, n = 1, warn = FALSE),
    error = function(e) NULL
  )
  try(file.remove(signal_file), silent = TRUE)

  if (is.null(content) || length(content) == 0 || nchar(content) == 0) {
    return(invisible(NULL))
  }

  dims <- suppressWarnings(
    as.numeric(strsplit(content, ",", fixed = TRUE)[[1]])
  )
  if (length(dims) != 2 || any(is.na(dims)) || any(dims <= 0)) {
    return(invisible(NULL))
  }

  rustgd_set_size(dims[1], dims[2])

  history <- .rustgd_state$plot_history
  n_plots <- length(history)
  if (n_plots > 0) {
    # Enter replay mode: new_page() will not advance the page counter,
    # so each plot's output goes to whichever file we direct via
    # rustgd_set_current_page() just before evaluating it. on.exit
    # restores the counter to the latest plot so the next user-issued
    # plot() increments cleanly to a fresh page.
    rustgd_set_replay_mode(TRUE)
    on.exit({
      rustgd_set_replay_mode(FALSE)
      rustgd_set_current_page(n_plots)
    }, add = TRUE)

    for (i in seq_len(n_plots)) {
      rustgd_set_current_page(i)
      plot_entry <- history[[i]]

      # Primary path: replay the recorded display list. This re-issues
      # the low-level drawing primitives onto the device at its new
      # size, which lets the plotting package's own layout choices
      # (legend placement, panel proportions, outer margins, etc.)
      # reflow naturally. No R-side construction code re-runs.
      # dev.flush() is called as a safety measure so the SVG is
      # written even if replayPlot does not trigger the device's
      # mode(0) callback on its own.
      used_recorded <- FALSE
      if (!is.null(plot_entry$recorded)) {
        used_recorded <- tryCatch(
          {
            grDevices::replayPlot(plot_entry$recorded)
            tryCatch(grDevices::dev.flush(), error = function(e) NULL)
            TRUE
          },
          error = function(e) FALSE
        )
      }

      # Fallback path: re-evaluate the captured expressions. This is
      # the original Path A behavior and runs only if recordPlot
      # returned NULL or replayPlot above threw an error. Behaves
      # like a fresh REPL evaluation, including the auto-print of
      # visible values that grid-based plots (ggplot, lattice, tmap)
      # rely on to trigger their draw side effects.
      if (!used_recorded) {
        exprs <- plot_entry$exprs
        if (is.null(exprs)) next
        for (e in exprs) {
          tryCatch({
            out <- withVisible(eval(e, envir = globalenv()))
            if (isTRUE(out$visible)) print(out$value)
          }, error = function(err) NULL)
        }
      }
    }
  }

  # Consume flags set by our replay so the task callback (which fires
  # for the user's commands, not our internal eval) doesn't get confused.
  rustgd_check_new_page()
  rustgd_check_drew()

  invisible(NULL)
}

#' Internal: snapshot the current device's graphics display list,
#' provided rustgd is the active device. Returns NULL silently in any
#' situation where recordPlot is not safe or not meaningful (e.g. the
#' user switched devices mid-expression, or there's nothing to
#' record). Recording is cheap compared to the actual drawing that
#' just happened, so calling it on every drew/new_page flag fire
#' (not just new_page) is fine.
.rustgd_record_current <- function() {
  rustgd_dev <- .rustgd_state$device_num
  if (is.null(rustgd_dev) || !(rustgd_dev %in% dev.list())) {
    return(NULL)
  }
  if (dev.cur() != rustgd_dev) {
    return(NULL)
  }
  tryCatch(grDevices::recordPlot(), error = function(e) NULL)
}

#' Internal: fired after every user top-level expression. If the
#' expression triggered a new page (e.g. plot()), open a fresh entry
#' in plot_history with the captured display list and the source
#' expression. If it just drew on the current page (e.g. lines() or
#' points() after a plot, or any update to the most recent ggplot),
#' update the existing entry's display list to reflect the new state
#' and append the expression to the entry's expression list.
#'
#' The recordPlot capture is the primary mechanism for resize replay.
#' Expressions are retained as a fallback in case recordPlot returned
#' NULL (rare, e.g. if the user switched devices mid-expression) or
#' replayPlot throws at replay time.
#' @noRd
rustgd_task_callback <- function(expr) {
  if (rustgd_check_new_page()) {
    recorded <- .rustgd_record_current()
    .rustgd_state$plot_history <- c(
      .rustgd_state$plot_history,
      list(list(exprs = list(expr), recorded = recorded))
    )
    rustgd_check_drew()
  } else if (rustgd_check_drew()) {
    n <- length(.rustgd_state$plot_history)
    if (n > 0) {
      recorded <- .rustgd_record_current()
      .rustgd_state$plot_history[[n]]$exprs <- c(
        .rustgd_state$plot_history[[n]]$exprs,
        list(expr)
      )
      # Only overwrite a non-NULL stored recording with a non-NULL
      # new one. A NULL here (e.g. transient dev.cur() mismatch)
      # shouldn't lose the previous successful capture.
      if (!is.null(recorded)) {
        .rustgd_state$plot_history[[n]]$recorded <- recorded
      }
    }
  }
  invisible(NULL)
}

#' Internal: process a clear-all signal from the viewer. Empties the
#' plot history, deletes every plot-NNNN.svg in the session dir, and
#' resets the page counter so the next user-issued plot() starts fresh
#' at plot-0001.svg.
rustgd_handle_clear_all <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) return(invisible(NULL))

  # Consume the signal first so we don't loop on it.
  try(file.remove(file.path(session_dir, "clear_all.txt")), silent = TRUE)

  .rustgd_state$plot_history <- list()

  svg_files <- list.files(
    session_dir,
    pattern = "^plot-[0-9]+\\.svg$",
    full.names = TRUE
  )
  if (length(svg_files) > 0) {
    try(file.remove(svg_files), silent = TRUE)
  }

  rustgd_set_current_page(0)
  invisible(NULL)
}

#' Internal: process a clear-active-plot signal from the viewer. The
#' marker file contains the 1-based index of the plot to remove. The
#' entry is dropped from plot_history, the corresponding SVG file is
#' deleted, and any higher-numbered files are renamed down by one so
#' the on-disk numbering stays contiguous and matches the new history
#' indices.
rustgd_handle_clear_plot <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) return(invisible(NULL))
  marker <- file.path(session_dir, "clear_plot.txt")

  content <- tryCatch(
    readLines(marker, n = 1, warn = FALSE),
    error = function(e) NULL
  )
  try(file.remove(marker), silent = TRUE)

  if (is.null(content) || length(content) == 0 || nchar(content) == 0) {
    return(invisible(NULL))
  }

  idx <- suppressWarnings(as.integer(content))
  if (is.na(idx) || idx < 1) return(invisible(NULL))

  n_plots <- length(.rustgd_state$plot_history)
  if (idx > n_plots) return(invisible(NULL))

  # Drop the entry from history.
  .rustgd_state$plot_history <- .rustgd_state$plot_history[-idx]

  # Delete the file for the removed plot.
  target_file <- file.path(session_dir, sprintf("plot-%04d.svg", idx))
  try(file.remove(target_file), silent = TRUE)

  # Shift higher-numbered files down by one so numbering stays
  # contiguous and aligned with the new history indices.
  if (idx < n_plots) {
    for (i in (idx + 1):n_plots) {
      old_file <- file.path(session_dir, sprintf("plot-%04d.svg", i))
      new_file <- file.path(session_dir, sprintf("plot-%04d.svg", i - 1))
      if (file.exists(old_file)) {
        try(file.rename(old_file, new_file), silent = TRUE)
      }
    }
  }

  # Update the page counter to match the new history length so the
  # next user-issued plot() increments to the right slot.
  rustgd_set_current_page(length(.rustgd_state$plot_history))
  invisible(NULL)
}

#' Use rustgd for plots, web content, and data frames.
#'
#' Activates the full rustgd suite in one call: the graphics device for
#' plots, the web viewer for HTML widgets and Shiny apps (and explicit
#' URLs via [rustgd_browse()]), and the data frame viewer behind `View()`.
#' The change takes
#' effect immediately in the current session, and a small snippet is also
#' written to your user-level `.Rprofile` so the same setup is restored
#' automatically in future sessions.
#'
#' Two startup modes control only the plot device:
#'
#' "lazy" (the default) registers rustgd as the default device via
#' `options(device = ...)`. The viewer window opens when you draw your
#' first plot and not before, matching base R's `quartz` and `X11`.
#'
#' "eager" additionally opens a plot window straight away, so it is ready
#' before you plot anything.
#'
#' The web viewer and the `View()` route are turned on immediately in
#' both modes. The `.Rprofile` snippet is guarded by `interactive()` so
#' non-interactive contexts (Rscript, R CMD BATCH, knitr, testthat,
#' package checks) are unaffected, and by `requireNamespace()` so
#' uninstalling rustgd will not break R startup.
#'
#' Safe to call repeatedly. Re-running with a different `mode` replaces
#' the existing snippet in place.
#'
#' @param mode Either "lazy" or "eager"; see description. Affects the
#'   plot device only.
#' @param rprofile_path Path to the `.Rprofile` to modify. Defaults to
#'   `~/.Rprofile`. Pass an explicit path to target a project-local
#'   profile instead.
#'
#' @seealso [unuse_rustgd()] to undo this, and [rustgd_enable()] for a
#'   one-session activation that does not touch `.Rprofile`.
#'
#' @export
use_rustgd <- function(
  mode = c("lazy", "eager"),
  rprofile_path = path.expand("~/.Rprofile")
) {
  mode <- match.arg(mode)

  # Apply to the running session right away.
  rustgd_enable(mode)

  # Persist for future sessions via a marked .Rprofile snippet.
  start_marker <- "# >>> rustgd auto-activate >>>"
  end_marker <- "# <<< rustgd auto-activate <<<"

  snippet <- c(
    start_marker,
    paste0("# mode: ", mode),
    "if (interactive() && requireNamespace(\"rustgd\", quietly = TRUE)) {",
    "  tryCatch(",
    paste0("    rustgd::rustgd_enable(\"", mode, "\"),"),
    "    error = function(e) message(\"rustgd: auto-activation failed: \", conditionMessage(e))",
    "  )",
    "}",
    end_marker
  )

  existing <- if (file.exists(rprofile_path)) {
    readLines(rprofile_path, warn = FALSE)
  } else {
    character(0)
  }

  stripped <- .rustgd_strip_snippet(existing, start_marker, end_marker)
  if (is.null(stripped)) {
    stop(
      "rustgd: ", rprofile_path, " contains a rustgd start marker but no ",
      "matching end marker. Please remove the broken snippet manually, ",
      "then re-run this function.",
      call. = FALSE
    )
  }

  # Trim trailing blank lines so the inserted snippet sits at a
  # predictable place with exactly one blank line of separation.
  while (length(stripped) > 0 && stripped[length(stripped)] == "") {
    stripped <- stripped[-length(stripped)]
  }

  new_contents <- if (length(stripped) == 0) {
    snippet
  } else {
    c(stripped, "", snippet)
  }

  writeLines(new_contents, rprofile_path)

  message("rustgd: active now (plots, web viewer, and View() data frames).")
  message(sprintf(
    "  Persisted to %s (mode: %s) for future sessions.",
    rprofile_path, mode
  ))
  message("  To undo everywhere: rustgd::unuse_rustgd()")

  invisible(rprofile_path)
}

#' Stop using rustgd for plots, web content, and data frames.
#'
#' Inverse of [use_rustgd()]. Restores the previous graphics device, web
#' viewer, and Shiny launcher in the current session, removes the rustgd
#' `View()` route, and strips the auto-activation snippet from
#' `.Rprofile`. Any rustgd windows already open are left alone. Safe to
#' call when nothing is active.
#'
#' @param rprofile_path Path to the `.Rprofile` to modify. Defaults to
#'   `~/.Rprofile`.
#'
#' @export
unuse_rustgd <- function(
  rprofile_path = path.expand("~/.Rprofile")
) {
  # Undo in the running session.
  rustgd_disable()

  start_marker <- "# >>> rustgd auto-activate >>>"
  end_marker <- "# <<< rustgd auto-activate <<<"

  if (!file.exists(rprofile_path)) {
    message("rustgd: deactivated in this session (no .Rprofile to clean).")
    return(invisible(FALSE))
  }

  existing <- readLines(rprofile_path, warn = FALSE)
  stripped <- .rustgd_strip_snippet(existing, start_marker, end_marker)

  if (is.null(stripped)) {
    stop(
      "rustgd: ", rprofile_path, " contains a rustgd start marker but no ",
      "matching end marker. Please remove the broken snippet manually.",
      call. = FALSE
    )
  }

  if (identical(stripped, existing)) {
    message(
      "rustgd: deactivated in this session (no snippet in ",
      rprofile_path, ")."
    )
    return(invisible(TRUE))
  }

  # Trim any trailing blank lines that may now be exposed.
  while (length(stripped) > 0 && stripped[length(stripped)] == "") {
    stripped <- stripped[-length(stripped)]
  }

  writeLines(stripped, rprofile_path)
  message("rustgd: deactivated and removed from ", rprofile_path)
  invisible(TRUE)
}

#' Activate the rustgd suite in the current session only.
#'
#' Routes plots to the rustgd graphics device, HTML widgets and Shiny
#' apps to the rustgd web viewer, and `View()` to the rustgd data frame
#' window, for this session only. Does not modify `.Rprofile`.
#' Used internally by [use_rustgd()] and by the `.Rprofile` snippet it
#' writes; also usable directly for a one-off session.
#'
#' @param mode Either "lazy" or "eager"; affects the plot device only.
#'   "eager" opens a plot window immediately.
#'
#' @seealso [rustgd_disable()], [use_rustgd()].
#'
#' @export
rustgd_enable <- function(mode = c("lazy", "eager")) {
  mode <- match.arg(mode)

  # Plots: route newly opened graphics devices to rustgd. Guard against
  # stashing our own function as the "previous" device on repeat calls.
  prev_device <- getOption("device")
  if (!identical(prev_device, rustgd)) {
    .rustgd_state$prev_device <- prev_device
  }
  options(device = rustgd)
  if (mode == "eager") {
    rustgd()
  }

  # HTML widgets (options(viewer)) and Shiny apps (shiny.launch.browser).
  use_rustgd_webview()

  # Data frames: send View() to the rustgd frames window.
  .rustgd_mask_view()

  invisible(NULL)
}

#' Deactivate the rustgd suite in the current session only.
#'
#' Inverse of [rustgd_enable()]. Restores the previous graphics device,
#' web viewer, and Shiny launcher, and removes the `View()` route, without
#' touching `.Rprofile`. Open rustgd windows are left alone.
#'
#' @export
rustgd_disable <- function() {
  # Plots: restore the previous device, falling back to the platform
  # default if we have nothing stored.
  prev_device <- .rustgd_state$prev_device
  if (is.null(prev_device)) {
    prev_device <- .rustgd_default_device()
  }
  options(device = prev_device)

  # Web viewer and Shiny launcher.
  unuse_rustgd_webview()

  # Data frames.
  .rustgd_unmask_view()

  invisible(NULL)
}

# Internal: the platform's stock interactive graphics device name, used
# as a restore fallback when no previous device was recorded.
.rustgd_default_device <- function() {
  sysname <- Sys.info()[["sysname"]]
  if (.Platform$OS.type == "windows") {
    "windows"
  } else if (identical(sysname, "Darwin")) {
    "quartz"
  } else {
    "X11"
  }
}

# Internal: route View(df) to the rustgd frames window by binding `View`
# in the global environment. The global environment is searched before any
# attached package, so this wins over utils::View regardless of when utils
# is attached. (A search-path attach() does not survive startup: when this
# runs from .Rprofile only base is loaded, and utils attaches afterward
# above the mask, so utils::View would win.) The wrapper deparses the
# variable name at this boundary (View() is called by the user, so
# substitute() sees their expression) and passes it through as the window
# title; otherwise every window would be titled after the wrapper's own
# argument. The binding is tagged so unmasking only ever removes our own
# View and never a View the user defined themselves.
.rustgd_mask_view <- function() {
  wrapper <- function(x, title = NULL) {
    if (is.null(title)) {
      title <- deparse(substitute(x))[1]
    }
    view(x, title = title)
  }
  attr(wrapper, "rustgd_view_mask") <- TRUE
  assign("View", wrapper, envir = globalenv())
  invisible(NULL)
}

# Internal: remove our View() binding from the global environment, leaving
# a user-defined View (one without our tag) untouched.
.rustgd_unmask_view <- function() {
  g <- globalenv()
  if (exists("View", envir = g, inherits = FALSE)) {
    cur <- get("View", envir = g, inherits = FALSE)
    if (is.function(cur) && isTRUE(attr(cur, "rustgd_view_mask"))) {
      rm("View", envir = g)
    }
  }
  invisible(NULL)
}

# Internal helper. Removes a single rustgd snippet block (including
# one blank line immediately above it, if present) from a vector of
# lines. Returns the unchanged vector if no snippet is found, or
# NULL if a start marker exists without a matching end marker.
.rustgd_strip_snippet <- function(lines, start_marker, end_marker) {
  start_idx <- which(lines == start_marker)
  if (length(start_idx) == 0) return(lines)

  end_idx <- which(lines == end_marker)
  end_idx <- end_idx[end_idx > start_idx[1]]
  if (length(end_idx) == 0) return(NULL)

  start_idx <- start_idx[1]
  end_idx <- end_idx[1]

  strip_start <- start_idx
  if (strip_start > 1 && lines[strip_start - 1] == "") {
    strip_start <- strip_start - 1
  }

  lines[-(strip_start:end_idx)]
}
