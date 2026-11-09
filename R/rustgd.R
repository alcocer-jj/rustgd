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

  # Stop the poll chain of any earlier device so two chains never run
  # side by side.
  cancel_poll <- .rustgd_state$poll_cancel
  .rustgd_state$poll_cancel <- NULL
  if (is.function(cancel_poll)) {
    tryCatch(cancel_poll(), error = function(e) NULL)
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
  .rustgd_state$visible <- NULL
  .rustgd_state$rebuild_paused <- FALSE
  .rustgd_state$active <- TRUE
  .rustgd_state$session_dir <- file.path(tempdir(), paste0("rustgd-", Sys.getpid()))

  # Each page is written as a .rgd display list, which the viewer draws
  # directly. With options(rustgd.debug_svg = TRUE) the device also writes
  # each page as SVG, and the viewer can flip between drawing either one.
  rustgd_set_debug_svg(isTRUE(getOption("rustgd.debug_svg", FALSE)))

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
  # It returns FALSE once the device has been retired, which tells R to
  # remove it. That is safe on every R version, unlike removing a
  # callback from inside itself.
  addTaskCallback(
    function(expr, value, ok, visible) {
      tryCatch(rustgd_task_callback(expr), error = function(e) NULL)
      isTRUE(.rustgd_state$active)
    },
    name = "rustgd_resize_capture"
  )

  # Schedule periodic resize check via the `later` package. The callback
  # fires during R's idle time and re-schedules itself.
  if (!requireNamespace("later", quietly = TRUE)) {
    warning("Package 'later' is required for automatic resize reflow. Install it with install.packages('later').")
    return(invisible(NULL))
  }
  .rustgd_state$poll_cancel <- later::later(rustgd_poll_resize, delay = 0.005)

  invisible(NULL)
}

# Package-level state.
.rustgd_state <- new.env(parent = emptyenv())
# plot_history is a list of plot entries, one per logical plot. Each
# entry is a named list with four fields.
#   - $recorded is the result of grDevices::recordPlot() captured right
#     after the plot was drawn. Replaying it is how a plot is redrawn at
#     a new size.
#   - $exprs holds the top-level expressions that produced the plot, one
#     for a ggplot and several for a layered base plot like plot() then
#     lines(). They are evaluated again only if the replay fails.
#   - $page is the device page the plot was drawn on, so the plot always
#     maps to its own page files, even after other plots are cleared.
#   - $size is the device size the plot was last drawn at. A plot whose
#     size differs from the device's current size is stale.
.rustgd_state$plot_history <- list()
.rustgd_state$active <- FALSE
.rustgd_state$device_num <- NULL
.rustgd_state$session_dir <- NULL
.rustgd_state$prev_device <- NULL
# Cancels the next scheduled poll. later::later() returns it.
.rustgd_state$poll_cancel <- NULL
# Page number of the plot the viewer is showing.
.rustgd_state$visible <- NULL
# Set by Ctrl+C during a redraw. Plots that are off screen then wait
# until they are viewed instead of being redrawn in the background.
.rustgd_state$rebuild_paused <- FALSE

#' Internal: tear down the rustgd session. Removes the task callback,
#' clears state, and optionally closes the R device. Idempotent;
#' safe to call from poll handlers that may race with manual dev.off().
#' @noRd
rustgd_shutdown <- function(close_device = TRUE, remove_callback = TRUE) {
  .rustgd_state$active <- FALSE

  # Cancel the next scheduled poll so nothing outlives the device.
  cancel_poll <- .rustgd_state$poll_cancel
  .rustgd_state$poll_cancel <- NULL
  if (is.function(cancel_poll)) {
    tryCatch(cancel_poll(), error = function(e) NULL)
  }

  if (close_device) {
    dev_num <- .rustgd_state$device_num
    if (!is.null(dev_num) && dev_num %in% dev.list()) {
      tryCatch(dev.off(dev_num), error = function(e) NULL)
    }
  }

  # The task callback itself passes remove_callback = FALSE. It then
  # returns FALSE and R removes it once it has finished running.
  if (remove_callback) {
    callbacks <- getTaskCallbackNames()
    if ("rustgd_resize_capture" %in% callbacks) {
      tryCatch(removeTaskCallback("rustgd_resize_capture"), error = function(e) NULL)
    }
  }

  .rustgd_state$plot_history <- list()
  .rustgd_state$device_num <- NULL
  invisible(NULL)
}

#' Internal. Scheduled poll for viewer signals. It handles a closed
#' window, Clear all, Clear plot, the plot on screen, and resizes, then
#' redraws at most one stale plot. It reschedules itself while the
#' device is active.
rustgd_poll_resize <- function() {
  if (!isTRUE(.rustgd_state$active)) {
    return(invisible(NULL))
  }

  # Schedule the next poll before doing any work. If anything below is
  # interrupted (Ctrl+C during a long replay) or fails, the chain still
  # carries on, so resize, Clear plot, and window close keep working.
  .rustgd_state$poll_cancel <- later::later(rustgd_poll_resize, delay = 0.005)

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
      tryCatch(
        rustgd_handle_clear_all(),
        error = function(e) NULL,
        interrupt = function(e) NULL
      )
    }

    # Clear-active signal: remove the specified plot from history
    # and renumber remaining files so the gallery stays contiguous.
    clear_plot_marker <- file.path(session_dir, "clear_plot.txt")
    if (file.exists(clear_plot_marker)) {
      tryCatch(
        rustgd_handle_clear_plot(),
        error = function(e) NULL,
        interrupt = function(e) NULL
      )
    }

    # The plot on screen. The viewer writes its page number to view.txt
    # whenever that changes, by paging or by jumping to a new plot.
    view_marker <- file.path(session_dir, "view.txt")
    if (file.exists(view_marker)) {
      content <- tryCatch(
        readLines(view_marker, n = 1, warn = FALSE),
        error = function(e) NULL
      )
      try(file.remove(view_marker), silent = TRUE)
      page <- suppressWarnings(as.integer(content))
      if (length(page) == 1 && !is.na(page) && page >= 1) {
        .rustgd_state$visible <- page
      }
    }
  }

  tryCatch(
    rustgd_input_handler(),
    error = function(e) NULL,
    interrupt = function(e) NULL
  )

  # Redraw at most one stale plot per poll, so R hands the console back
  # between plots. Ctrl+C pauses the plots that are off screen until they
  # are viewed. A redraw that is busy in C or Rust code finishes first.
  tryCatch(
    rustgd_rebuild_next(),
    error = function(e) NULL,
    interrupt = function(e) {
      .rustgd_state$rebuild_paused <- TRUE
      NULL
    }
  )
  invisible(NULL)
}

#' Internal. Applies a pending resize from resize.txt. The file holds the
#' new width and height and, after them, the page number of the plot on
#' screen. Nothing is redrawn here. Every plot drawn at another size is
#' now stale, and rustgd_rebuild_next() redraws them one per poll,
#' starting with the plot on screen.
rustgd_input_handler <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) {
    return(invisible(FALSE))
  }
  signal_file <- file.path(session_dir, "resize.txt")
  if (!file.exists(signal_file)) {
    return(invisible(FALSE))
  }

  content <- tryCatch(
    readLines(signal_file, n = 1, warn = FALSE),
    error = function(e) NULL
  )
  try(file.remove(signal_file), silent = TRUE)

  if (is.null(content) || length(content) == 0 || nchar(content) == 0) {
    return(invisible(FALSE))
  }

  parts <- suppressWarnings(
    as.numeric(strsplit(content, ",", fixed = TRUE)[[1]])
  )
  if (length(parts) < 2 || any(is.na(parts[1:2])) || any(parts[1:2] <= 0)) {
    return(invisible(FALSE))
  }

  # rustgd_set_size() acts on the current device. Make rustgd's device
  # current for it and put back whatever was current, so a png() or pdf()
  # the user has open is never resized.
  prev_dev <- dev.cur()
  dev_num <- .rustgd_state$device_num
  if (prev_dev != dev_num) {
    grDevices::dev.set(dev_num)
    on.exit(if (prev_dev %in% dev.list()) grDevices::dev.set(prev_dev), add = TRUE)
  }
  rustgd_set_size(parts[1], parts[2])

  if (length(parts) >= 3 && !is.na(parts[3]) && parts[3] >= 1) {
    .rustgd_state$visible <- as.integer(parts[3])
  }
  # A new size restarts the redraws, which also lifts a pause from Ctrl+C.
  .rustgd_state$rebuild_paused <- FALSE
  invisible(TRUE)
}

#' Internal. Redraws one stale plot, the one most worth redrawing. The plot
#' on screen comes first. Next is the plot the device itself is on, so a
#' later lines() or points() lands on an up-to-date plot. After those,
#' plots go in order of distance from the one on screen, unless Ctrl+C
#' paused them.
#' @noRd
rustgd_rebuild_next <- function() {
  history <- .rustgd_state$plot_history
  if (length(history) == 0) {
    return(invisible(FALSE))
  }
  size <- rustgd_device_size()
  stale <- which(vapply(
    history,
    function(entry) !(length(entry$size) == 2 && all(entry$size == size)),
    logical(1)
  ))
  if (length(stale) == 0) {
    return(invisible(FALSE))
  }

  pages <- vapply(history[stale], function(entry) as.integer(entry$page), integer(1))
  owner <- rustgd_current_page()
  visible <- .rustgd_state$visible
  if (is.null(visible)) {
    visible <- owner
  }

  if (visible %in% pages) {
    i <- stale[match(visible, pages)]
  } else if (owner %in% pages) {
    i <- stale[match(owner, pages)]
  } else if (isTRUE(.rustgd_state$rebuild_paused)) {
    return(invisible(FALSE))
  } else {
    i <- stale[which.min(abs(pages - visible))]
  }
  entry <- history[[i]]

  # Redrawing acts on the current device. Make rustgd's device current and
  # put back whatever was current.
  prev_dev <- dev.cur()
  dev_num <- .rustgd_state$device_num
  if (prev_dev != dev_num) {
    grDevices::dev.set(dev_num)
    on.exit(if (prev_dev %in% dev.list()) grDevices::dev.set(prev_dev), add = TRUE)
  }

  tryCatch(
    if (isTRUE(entry$page == owner)) {
      .rustgd_redraw_on_device(entry)
    } else {
      .rustgd_redraw_offscreen(entry, size)
    },
    error = function(e) NULL
  )

  # Mark it drawn at this size even if the redraw failed, so a broken plot
  # is retried on the next resize rather than on every poll. An interrupt
  # skips this line and leaves the plot stale.
  .rustgd_state$plot_history[[i]]$size <- size

  # Most of a replay runs in C and Rust code, where R does not check for
  # Ctrl+C. A Ctrl+C pressed then stays pending, and R would notice it
  # only after this poll returns, too late to pause the remaining plots.
  # Sys.sleep(0) makes R check now, while the poll's interrupt handler
  # can still catch it.
  Sys.sleep(0)
  invisible(TRUE)
}

#' Internal. Replays one recorded plot onto the current device, where its
#' own layout reflows to the device's size. If the recording is missing or
#' fails to replay, the plot's expressions are evaluated again instead,
#' which behaves like a fresh evaluation at the console, including the
#' auto-print that ggplot and lattice rely on.
#' @noRd
.rustgd_replay_entry <- function(entry) {
  used_recorded <- FALSE
  if (!is.null(entry$recorded)) {
    used_recorded <- tryCatch(
      {
        grDevices::replayPlot(entry$recorded)
        tryCatch(grDevices::dev.flush(), error = function(e) NULL)
        TRUE
      },
      error = function(e) FALSE
    )
  }
  if (!used_recorded && !is.null(entry$exprs)) {
    for (e in entry$exprs) {
      tryCatch(
        {
          out <- withVisible(eval(e, envir = globalenv()))
          if (isTRUE(out$visible)) print(out$value)
        },
        error = function(err) NULL
      )
    }
  }
  invisible(NULL)
}

#' Internal. Redraws the plot that owns the device's current page on the
#' device itself, so the device's display list and graphics state end up
#' at the new size too. Replay mode keeps the page counter still, so the
#' output overwrites the plot's own file.
#' @noRd
.rustgd_redraw_on_device <- function(entry) {
  owner <- rustgd_current_page()
  rustgd_set_replay_mode(TRUE)
  on.exit(
    {
      rustgd_set_replay_mode(FALSE)
      rustgd_set_current_page(owner)
      # Consume the flags the replay set, so the task callback does not
      # mistake them for a new plot from the user.
      rustgd_check_new_page()
      rustgd_check_drew()
    },
    add = TRUE
  )
  rustgd_set_current_page(as.integer(entry$page))
  .rustgd_replay_entry(entry)
  invisible(NULL)
}

#' Internal. Redraws any other plot on a short-lived second device that
#' writes straight to that plot's file and opens no window. The main
#' device keeps its display list and graphics state, so a later lines()
#' still adds to the plot it belongs to.
#' @noRd
.rustgd_redraw_offscreen <- function(entry, size) {
  main <- dev.cur()
  on.exit(
    {
      # Close the second device by its name, which also covers an
      # interrupt that lands before its number could be stored.
      devices <- dev.list()
      for (d in devices[names(devices) == "rustgd_offscreen"]) {
        grDevices::dev.off(d)
      }
      if (main %in% dev.list()) {
        grDevices::dev.set(main)
      }
      rustgd_check_new_page()
      rustgd_check_drew()
    },
    add = TRUE
  )
  rustgd_open_offscreen(as.integer(entry$page), size[1], size[2])
  .rustgd_replay_entry(entry)
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
  # Catch a window close the poll missed, for example one made while a
  # long replay was running. Retiring the device here means the next
  # plot opens a fresh window instead of drawing into a closed one.
  session_dir <- .rustgd_state$session_dir
  if (isTRUE(.rustgd_state$active) && !is.null(session_dir) &&
    file.exists(file.path(session_dir, "viewer_closed"))) {
    rustgd_shutdown(close_device = TRUE, remove_callback = FALSE)
    return(invisible(NULL))
  }

  # If a plot arrived after the window was closed but before R noticed,
  # the device reopened the window and deleted the old plots. Drop the
  # matching history so closing the window always starts over.
  if (rustgd_take_gallery_reset()) {
    .rustgd_state$plot_history <- list()
    .rustgd_state$visible <- NULL
  }

  if (rustgd_check_new_page()) {
    recorded <- .rustgd_record_current()
    page <- rustgd_current_page()
    .rustgd_state$plot_history <- c(
      .rustgd_state$plot_history,
      list(list(
        exprs = list(expr),
        recorded = recorded,
        page = page,
        size = rustgd_device_size()
      ))
    )
    # The viewer jumps to a new plot, so it is the one on screen.
    .rustgd_state$visible <- page
    rustgd_check_drew()
  } else if (rustgd_check_drew()) {
    # Additions such as lines() land on the page the device is on, so
    # they belong to the entry that owns that page.
    page <- rustgd_current_page()
    owns <- which(vapply(
      .rustgd_state$plot_history,
      function(entry) isTRUE(entry$page == page),
      logical(1)
    ))
    if (length(owns) > 0) {
      n <- owns[length(owns)]
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
      .rustgd_state$plot_history[[n]]$size <- rustgd_device_size()
    }
  }
  invisible(NULL)
}

#' Internal. Processes a clear-all signal from the viewer. Empties the
#' plot history, deletes every page file in the session dir, and resets
#' the page counter so the next user-issued plot() starts fresh at
#' page 1.
rustgd_handle_clear_all <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) {
    return(invisible(NULL))
  }

  # Consume the signal first so we don't loop on it.
  try(file.remove(file.path(session_dir, "clear_all.txt")), silent = TRUE)

  .rustgd_state$plot_history <- list()
  .rustgd_state$visible <- NULL
  .rustgd_state$rebuild_paused <- FALSE

  page_files <- list.files(
    session_dir,
    pattern = "^plot-[0-9]+\\.(svg|rgd)$",
    full.names = TRUE
  )
  if (length(page_files) > 0) {
    try(file.remove(page_files), silent = TRUE)
  }

  rustgd_set_current_page(0)
  invisible(NULL)
}

#' Internal. Checks every page written with options(rustgd.debug_svg =
#' TRUE). Each .rgd file is decoded and turned back into SVG, which must
#' match the .svg written beside it byte for byte. Prints a summary and
#' returns the pages that differ.
#' @noRd
.rustgd_check_binary <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir) || !dir.exists(session_dir)) {
    message("No rustgd session folder yet. Draw a plot first.")
    return(invisible(integer(0)))
  }
  result <- rustgd_binary_check(session_dir)
  if (result$checked == 0) {
    message(
      "No page has both an .rgd and an .svg to compare. Set ",
      "options(rustgd.debug_svg = TRUE) before the first plot to write both."
    )
    return(invisible(integer(0)))
  }
  bad <- result$mismatched
  message(sprintf(
    "Checked %d .rgd pages. %d differ from their .svg.",
    result$checked,
    length(bad)
  ))
  invisible(bad)
}

#' Internal. Processes a clear-plot signal from the viewer. The marker
#' file holds the page number of the plot to remove. Its file is deleted,
#' every higher page moves down by one, and plot_history changes the same
#' way, so files and history stay in step.
rustgd_handle_clear_plot <- function() {
  session_dir <- .rustgd_state$session_dir
  if (is.null(session_dir)) {
    return(invisible(NULL))
  }
  marker <- file.path(session_dir, "clear_plot.txt")

  content <- tryCatch(
    readLines(marker, n = 1, warn = FALSE),
    error = function(e) NULL
  )
  try(file.remove(marker), silent = TRUE)

  if (is.null(content) || length(content) == 0 || nchar(content) == 0) {
    return(invisible(NULL))
  }

  page <- suppressWarnings(as.integer(content))
  if (is.na(page) || page < 1) {
    return(invisible(NULL))
  }

  # A page has an .svg file, and an .rgd file too in binary mode.
  files <- list.files(session_dir, pattern = "^plot-[0-9]+\\.(svg|rgd)$")
  file_pages <- as.integer(sub("^plot-([0-9]+)\\.(svg|rgd)$", "\\1", files))
  if (!(page %in% file_pages)) {
    return(invisible(NULL))
  }

  # Delete the plot's files, then move every higher page down by one.
  for (ext in c("svg", "rgd")) {
    target <- file.path(session_dir, sprintf("plot-%04d.%s", page, ext))
    if (file.exists(target)) {
      try(file.remove(target), silent = TRUE)
    }
  }
  for (p in sort(unique(file_pages[file_pages > page]))) {
    for (ext in c("svg", "rgd")) {
      old_file <- file.path(session_dir, sprintf("plot-%04d.%s", p, ext))
      if (file.exists(old_file)) {
        try(
          file.rename(
            old_file,
            file.path(session_dir, sprintf("plot-%04d.%s", p - 1L, ext))
          ),
          silent = TRUE
        )
      }
    }
  }

  # The history changes the same way.
  history <- .rustgd_state$plot_history
  keep <- vapply(history, function(entry) !isTRUE(entry$page == page), logical(1))
  history <- history[keep]
  for (i in seq_along(history)) {
    if (isTRUE(history[[i]]$page > page)) {
      history[[i]]$page <- history[[i]]$page - 1L
    }
  }
  .rustgd_state$plot_history <- history

  # So do the device's own page counter and the plot on screen.
  owner <- rustgd_current_page()
  if (owner >= page) {
    rustgd_set_current_page(owner - 1L)
  }
  visible <- .rustgd_state$visible
  if (!is.null(visible) && visible > page) {
    .rustgd_state$visible <- visible - 1L
  }
  invisible(NULL)
}

#' Use rustgd for plots, web content, and data frames.
#'
#' Activates the full rustgd suite for the current session: the graphics
#' device for plots, the web viewer for HTML widgets and Shiny apps (and
#' explicit URLs via [rustgd_browse()]). The data-frame viewer behind
#' `View()` and lowercase `view()` is always available once the package is
#' attached with `library(rustgd)`, so it does not require this call.
#'
#' To make activation persist across sessions, this function can add a
#' small snippet to your user-level `.Rprofile`. The snippet attaches the
#' package (so `View()` and `view()` come with it) and turns on the plot
#' device and web viewer, restoring the full suite at startup. Because
#' that edits a file in your home directory, it is only done in an
#' interactive session and only after you explicitly confirm at a prompt.
#' If you decline, or if the session is non-interactive (Rscript, R CMD
#' BATCH, knitr, testthat, package checks), nothing is written and the
#' snippet is printed for you to add yourself.
#'
#' Two startup modes control the plot device:
#'
#' "lazy" (the default) registers rustgd as the default device via
#' `options(device = ...)`. The viewer window opens when you draw your
#' first plot and not before, matching base R's `quartz` and `X11`.
#'
#' "eager" additionally opens a plot window straight away, so it is ready
#' before you plot anything.
#'
#' @param mode Either "lazy" or "eager"; see description. Affects the
#'   plot device only.
#' @param rprofile_path Path to the `.Rprofile` to modify. Defaults to
#'   `~/.Rprofile`. Pass an explicit path to target a project-local
#'   profile instead.
#'
#' @return Invisibly, the `.Rprofile` path if it was written, otherwise
#'   `FALSE`.
#'
#' @seealso [unuse_rustgd()] to undo this, and [rustgd_enable()] for a
#'   one-session activation that never touches `.Rprofile`.
#'
#' @export
use_rustgd <- function(
  mode = c("lazy", "eager"),
  rprofile_path = path.expand("~/.Rprofile")
) {
  mode <- match.arg(mode)

  # Apply to the running session right away.
  rustgd_enable(mode)

  # The snippet that reproduces this activation at interactive startup.
  start_marker <- "# >>> rustgd auto-activate >>>"
  end_marker <- "# <<< rustgd auto-activate <<<"

  snippet <- c(
    start_marker,
    paste0("# mode: ", mode),
    "if (interactive() && requireNamespace(\"rustgd\", quietly = TRUE)) {",
    "  tryCatch({",
    "    suppressPackageStartupMessages(library(rustgd))",
    paste0("    rustgd::rustgd_enable(\"", mode, "\")"),
    "  },",
    "    error = function(e) message(\"rustgd: auto-activation failed: \", conditionMessage(e))",
    "  )",
    "}",
    end_marker
  )

  message("rustgd: active for this session (plots, web viewer, and View()/view()).")

  # Persisting means editing the user's .Rprofile. Never do that during a
  # non-interactive run (R CMD check, Rscript, knitr); just show the lines.
  if (!interactive()) {
    message("  To make this permanent, add these lines to ", rprofile_path, ":")
    message(paste0("    ", snippet, collapse = "\n"))
    return(invisible(FALSE))
  }

  # Interactive: write only after explicit, affirmative confirmation.
  consent <- tryCatch(
    isTRUE(utils::askYesNo(
      sprintf("Write a rustgd auto-activation snippet to %s?", rprofile_path),
      default = FALSE
    )),
    error = function(e) FALSE
  )

  if (!consent) {
    message("  Left ", rprofile_path, " untouched. To persist it yourself, add:")
    message(paste0("    ", snippet, collapse = "\n"))
    return(invisible(FALSE))
  }

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
  message(sprintf("  Persisted to %s (mode: %s).", rprofile_path, mode))
  message("  To undo everywhere: rustgd::unuse_rustgd()")
  invisible(rprofile_path)
}

#' Stop using rustgd for plots, web content, and data frames.
#'
#' Inverse of [use_rustgd()]. Restores the previous graphics device, web
#' viewer, and Shiny launcher in the current session, removes the rustgd
#' `View()` and `view()` routes, and strips the auto-activation snippet from
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

  # Removing the snippet edits the user's .Rprofile, so, like use_rustgd(),
  # only do it interactively and only with explicit confirmation.
  if (!interactive()) {
    message("rustgd: deactivated in this session. A snippet remains in ", rprofile_path, ";")
    message("  delete the block between the rustgd markers to disable it at startup.")
    return(invisible(FALSE))
  }

  consent <- tryCatch(
    isTRUE(utils::askYesNo(
      sprintf("Remove the rustgd auto-activation snippet from %s?", rprofile_path),
      default = FALSE
    )),
    error = function(e) FALSE
  )

  if (!consent) {
    message("rustgd: deactivated in this session; left ", rprofile_path, " untouched.")
    return(invisible(FALSE))
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
#' apps to the rustgd web viewer, and `View()` and lowercase `view()` to
#' the rustgd data frame window, for this session only. Does not modify
#' `.Rprofile`.
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
  # View()/view() are ordinary package exports and need no activation here.
  use_rustgd_webview()

  invisible(NULL)
}

#' Deactivate the rustgd suite in the current session only.
#'
#' Inverse of [rustgd_enable()]. Restores the previous graphics device,
#' web viewer, and Shiny launcher, and removes the `View()` and `view()`
#' routes, without touching `.Rprofile`. Open rustgd windows are left alone.
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

#' @rdname view
#' @export
View <- function(df, title = NULL) {
  if (is.null(title)) {
    title <- deparse(substitute(df))[1]
  }
  view(df, title = title)
}

# Internal helper. Removes a single rustgd snippet block (including
# one blank line immediately above it, if present) from a vector of
# lines. Returns the unchanged vector if no snippet is found, or
# NULL if a start marker exists without a matching end marker.
.rustgd_strip_snippet <- function(lines, start_marker, end_marker) {
  start_idx <- which(lines == start_marker)
  if (length(start_idx) == 0) {
    return(lines)
  }

  end_idx <- which(lines == end_marker)
  end_idx <- end_idx[end_idx > start_idx[1]]
  if (length(end_idx) == 0) {
    return(NULL)
  }

  start_idx <- start_idx[1]
  end_idx <- end_idx[1]

  strip_start <- start_idx
  if (strip_start > 1 && lines[strip_start - 1] == "") {
    strip_start <- strip_start - 1
  }

  lines[-(strip_start:end_idx)]
}


# Shown when the package is attached with library(rustgd). Concise and
# once per session; suppressPackageStartupMessages() silences it. NOTE: if
# the package gains an .onAttach elsewhere (e.g. R/zzz.R), merge this body
# into that one rather than declaring a second .onAttach.
.onAttach <- function(libname, pkgname) {
  packageStartupMessage(
    "rustgd: View() and view() now open data frames in a rustgd window.\n",
    "  For plots and the web viewer this session, run rustgd::rustgd_enable().\n",
    "  To make that automatic, add to your ~/.Rprofile:\n",
    "    if (interactive() && requireNamespace(\"rustgd\", quietly = TRUE)) rustgd::rustgd_enable()\n",
    "  or run rustgd::use_rustgd() to be prompted to write it for you.\n",
    "  Silence this with suppressPackageStartupMessages(library(rustgd))."
  )
}
