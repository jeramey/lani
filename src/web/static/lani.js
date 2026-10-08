// LANI: live updates on review pages (docs/design.md §13.5), and line selection with the
// new-issue form in the code views (§13.4).
//
// Live updates: a review page listens to the review's event stream. When the review changes,
// the page fetches itself again and replaces each part marked data-live whose HTML the server
// changed, unless someone is editing in that part; then it shows a notice there instead.
//
// Line selection: click a line number to select the line; shift-click another on the same side
// to select the range. The URL hash follows the selection (#L42-L48 in a file, #<file>-H42-H48
// or -B… in the changes), so a selection can be linked to. The page's <template> elements hold
// the forms; this script only fills them in.
//
// The script adds no inline code or styles, so it runs under the CSP.
"use strict";

(function () {
  // Each live part's HTML as the server last sent it, by id. Taken before this script changes
  // anything on the page (a selection adds classes), so that only the server's changes count.
  const served = new Map();
  for (const part of document.querySelectorAll("[data-live][id]")) {
    served.set(part.id, part.outerHTML);
  }

  // The current selection: the table, the side ("head" or "base"), the line first clicked, and
  // the selected range.
  let selection = null;

  function lineRows(table) {
    return Array.from(table.querySelectorAll("tr[data-line]"));
  }

  function sideOf(row) {
    return row.dataset.side || "head";
  }

  function clearSelection() {
    document.querySelectorAll("tr.selected").forEach((row) => row.classList.remove("selected"));
    document.querySelectorAll("tr.new-issue-bar").forEach((row) => row.remove());
    selection = null;
  }

  function hashFor(table, side, start, end) {
    const file = table.closest(".diff-file");
    if (file) {
      const letter = side === "base" ? "B" : "H";
      return "#" + file.id + "-" + letter + start + (end !== start ? "-" + letter + end : "");
    }
    return "#L" + start + (end !== start ? "-L" + end : "");
  }

  function rangeText(start, end) {
    return start === end ? "line " + start : "lines " + start + "–" + end;
  }

  function select(table, side, anchorLine, line, updateHash) {
    const start = Math.min(anchorLine, line);
    const end = Math.max(anchorLine, line);
    clearSelection();
    selection = { table, side, anchorLine, start, end };
    let last = null;
    for (const row of lineRows(table)) {
      const number = Number(row.dataset.line);
      if (sideOf(row) === side && number >= start && number <= end) {
        row.classList.add("selected");
        last = row;
      }
    }
    if (updateHash) {
      history.replaceState(null, "", hashFor(table, side, start, end));
    }
    if (last) {
      showBar(last);
    }
    return last;
  }

  // A row after the selection with a button that opens the new-issue form.
  function showBar(lastRow) {
    const template = document.getElementById("new-issue-bar");
    if (!template) {
      return; // The review is closed.
    }
    const bar = template.content.firstElementChild.cloneNode(true);
    bar.querySelector("td").colSpan = lastRow.cells.length;
    const side = selection.side === "base" ? " (base side)" : "";
    bar.querySelector(".range").textContent = rangeText(selection.start, selection.end) + side;
    bar.querySelector("button").addEventListener("click", () => openForm(bar));
    lastRow.after(bar);
  }

  function openForm(bar) {
    const { table, side, start, end } = selection;
    const row = document.getElementById("new-issue-template").content.firstElementChild.cloneNode(true);
    row.querySelector("td").colSpan = bar.querySelector("td").colSpan;
    const form = row.querySelector("form");
    const path =
      table.dataset.path || (side === "base" ? table.dataset.basePath : table.dataset.headPath);
    const values = {
      path,
      side,
      start_line: start,
      end_line: end,
      commit: table.dataset.commit,
      merge_base: side === "base" ? table.dataset.mergeBase : "",
      back: location.pathname + location.search,
    };
    for (const [name, value] of Object.entries(values)) {
      form.elements[name].value = value;
    }
    const sideText = side === "base" ? " (base side)" : "";
    row.querySelector(".new-issue-title").textContent =
      "New issue on " + path + ", " + rangeText(start, end) + sideText;
    setUpEditor(form);
    row.querySelector(".cancel").addEventListener("click", () => row.remove());
    bar.replaceWith(row);
    form.elements.title.focus();
  }

  // Write and Preview tabs: the preview is rendered by the server, exactly as it will be shown.
  function setUpEditor(form) {
    const textarea = form.elements.body;
    const preview = form.querySelector(".preview");
    for (const tab of form.querySelectorAll(".editor-tabs button")) {
      tab.addEventListener("click", async () => {
        for (const other of form.querySelectorAll(".editor-tabs button")) {
          other.classList.toggle("active", other === tab);
        }
        const showPreview = tab.dataset.tab === "preview";
        textarea.hidden = showPreview;
        preview.hidden = !showPreview;
        if (showPreview) {
          preview.textContent = "Rendering…";
          try {
            const response = await fetch("/preview", {
              method: "POST",
              headers: { "Content-Type": "application/x-www-form-urlencoded" },
              body: new URLSearchParams({ body: textarea.value }),
              credentials: "same-origin",
            });
            if (!response.ok) {
              throw new Error("HTTP " + response.status);
            }
            // Safe to insert: the server renders Markdown with raw HTML disabled (§13.7).
            preview.innerHTML = (await response.text()) || "<p><em>Nothing to preview.</em></p>";
          } catch (error) {
            preview.textContent = "The preview could not be rendered: " + error.message;
          }
        }
      });
    }
  }

  document.addEventListener("click", (event) => {
    const link = event.target.closest("table.code-view td.ln a");
    if (!link) {
      return;
    }
    const row = link.closest("tr[data-line]");
    const table = link.closest("table.code-view");
    if (!row || !table) {
      return;
    }
    event.preventDefault();
    const side = sideOf(row);
    const line = Number(row.dataset.line);
    if (event.shiftKey && selection && selection.table === table && selection.side === side) {
      select(table, side, selection.anchorLine, line, true);
    } else {
      select(table, side, line, line, true);
    }
  });

  // Restore a selection from the URL hash when the page loads.
  function selectFromHash() {
    const hash = decodeURIComponent(location.hash);
    let match = hash.match(/^#L(\d+)(?:-L(\d+))?$/);
    if (match) {
      const table = document.querySelector("table.code-view");
      return table && select(table, "head", Number(match[1]), Number(match[2] || match[1]), false);
    }
    match = hash.match(/^#(file-[0-9a-f]+)-([HB])(\d+)(?:-[HB](\d+))?$/);
    if (match) {
      const file = document.getElementById(match[1]);
      const table = file && file.querySelector("table.code-view");
      const side = match[2] === "B" ? "base" : "head";
      return table && select(table, side, Number(match[3]), Number(match[4] || match[3]), false);
    }
    return null;
  }

  function selectFromHashAndScroll() {
    const last = selectFromHash();
    if (last) {
      const first = selection.table.querySelector("tr.selected");
      (first || last).scrollIntoView({ block: "center" });
    }
  }

  selectFromHashAndScroll();
  // Following a link to another selection on the same page changes only the hash. Our own
  // history.replaceState calls do not fire this event.
  window.addEventListener("hashchange", selectFromHashAndScroll);

  // ── Live updates ──────────────────────────────────────────────────────────────────────────

  const source = document.getElementById("live-source");
  // The latest change this page reflects: the event it was rendered with, then the last one the
  // stream announced.
  let lastEvent = source ? source.dataset.after : "";
  let stream = null;
  // Set once the server has said it stopped, or that this page can no longer be updated.
  let ended = false;
  // Failed attempts in a row to reach the server. The first can be a dropped connection that the
  // browser restores at once; the second means the server is gone. A page that was hidden when
  // the server stopped never heard `stopping`, so this is how it finds out.
  let failures = 0;
  const UNREACHABLE_AFTER = 2;

  // Only a visible page keeps a stream open: browsers allow a handful of connections per host,
  // and every open stream holds one. A page shown again catches up through `after`.
  function connect() {
    if (!source || stream || ended || document.hidden) {
      return;
    }
    stream = new EventSource(source.dataset.events + "?after=" + encodeURIComponent(lastEvent));
    stream.addEventListener("changed", (event) => {
      if (event.lastEventId) {
        lastEvent = event.lastEventId;
      }
      refresh();
    });
    for (const name of ["stopping", "failed"]) {
      stream.addEventListener(name, (event) => {
        ended = true;
        disconnect();
        showNotice("ended", event.data);
      });
    }
    stream.addEventListener("open", () => {
      failures = 0;
      clearNotice("unreachable");
    });
    stream.addEventListener("error", (event) => {
      if (event.target.readyState === EventSource.CLOSED) {
        // The server answered, but not with a stream, so the browser gives up.
        ended = true;
        disconnect();
        showNotice("ended", "Live updates could not start. Reload the page to see why.");
        return;
      }
      // The browser keeps trying to reconnect.
      failures += 1;
      if (failures >= UNREACHABLE_AFTER) {
        showNotice(
          "unreachable",
          "LANI cannot be reached; it may have stopped. Run lani open, or ask Claude to open it again."
        );
      }
    });
  }

  function disconnect() {
    if (stream) {
      stream.close();
      stream = null;
    }
  }

  // Refreshes run one at a time; a change announced during one starts another after it.
  let refreshing = false;
  let refreshAgain = false;
  // The page as last fetched, for parts left stale until nobody is editing in them.
  let latest = null;

  async function refresh() {
    if (refreshing) {
      refreshAgain = true;
      return;
    }
    refreshing = true;
    try {
      do {
        refreshAgain = false;
        const response = await fetch(location.pathname + location.search, {
          credentials: "same-origin",
        });
        if (!response.ok) {
          showNotice(
            "refresh",
            "This page could not be updated (HTTP " + response.status + "). Reload it to see why."
          );
          return;
        }
        latest = new DOMParser().parseFromString(await response.text(), "text/html");
        update(latest);
        clearNotice("refresh");
      } while (refreshAgain);
    } catch (error) {
      showNotice(
        "refresh",
        "This page could not be updated: " + error.message + ". Reload it to try again."
      );
    } finally {
      refreshing = false;
    }
  }

  // Replace the live parts the server changed, except those someone is editing in.
  function update(fresh) {
    for (const part of document.querySelectorAll("[data-live][id]")) {
      const replacement = fresh.getElementById(part.id);
      if (!replacement || replacement.outerHTML === served.get(part.id)) {
        continue;
      }
      if (beingEdited(part)) {
        markStale(part);
        continue;
      }
      served.set(part.id, replacement.outerHTML);
      // A copy: the fetched page stays whole, to be applied again (retryStale).
      part.replaceWith(document.importNode(replacement, true));
    }
    // Parts are replaced, never added or removed: a file that a new commit brings into the
    // Changes view, or takes out of it, would land in the wrong place. The page says so instead.
    const ids = (page) => Array.from(page.querySelectorAll("[data-live][id]"), (part) => part.id);
    if (ids(document).join("\n") === ids(fresh).join("\n")) {
      clearNotice("reshaped");
    } else {
      showNotice(
        "reshaped",
        "Parts of this page, such as the changed files, were added or removed elsewhere. " +
          "Reload to see them."
      );
    }
    // A replaced code table loses its selection; take it from the URL again.
    if (selection && !document.contains(selection.table)) {
      selectFromHash();
    }
  }

  // Whether replacing `part` could lose someone's work: a field in it has focus or unsaved
  // changes, or it has an open editor or new-issue form.
  function beingEdited(part) {
    const active = document.activeElement;
    if (active && part.contains(active) && active.matches("input, textarea, select")) {
      return true;
    }
    if (part.querySelector("details.inline-edit[open], tr.new-issue-row")) {
      return true;
    }
    for (const field of part.querySelectorAll("textarea, input[type=text]")) {
      if (field.value !== field.defaultValue) {
        return true;
      }
    }
    for (const select of part.querySelectorAll("select")) {
      if (selectChanged(select)) {
        return true;
      }
    }
    return false;
  }

  // Whether a select shows something other than what the page loaded with. A drop-down without
  // a `selected` option shows its first enabled option, which the browser marks selected itself,
  // so its options alone cannot tell.
  function selectChanged(select) {
    const options = Array.from(select.options);
    if (select.multiple) {
      return options.some((option) => option.selected !== option.defaultSelected);
    }
    let shown = -1;
    for (const option of options) {
      if (option.defaultSelected) {
        shown = option.index;
      }
    }
    if (shown === -1) {
      shown = options.findIndex((option) => !option.disabled);
    }
    return select.selectedIndex !== shown;
  }

  // A stale part catches up as soon as nobody is editing in it: the draft was cleared and the
  // field left, or the editor closed.
  function retryStale(event) {
    const part = event.target.closest && event.target.closest("[data-live]");
    if (!latest || !part || !part.querySelector(":scope > .stale-notice")) {
      return;
    }
    // During focusout, the focus has not moved on yet.
    setTimeout(() => update(latest), 0);
  }
  document.addEventListener("focusout", retryStale);
  // `toggle` does not bubble.
  document.addEventListener("toggle", retryStale, true);

  function markStale(part) {
    if (part.querySelector(":scope > .stale-notice")) {
      return;
    }
    const notice = document.createElement("p");
    notice.className = "notice stale-notice";
    const reload = document.createElement("button");
    reload.type = "button";
    reload.className = "link";
    reload.textContent = "Reload";
    reload.addEventListener("click", () => location.reload());
    notice.append("Updated elsewhere. ", reload, " to see the changes; copy anything you are writing first.");
    part.prepend(notice);
  }

  // Notices above the page, at most one of each kind:
  // - "ended": the server stopped, or this page can no longer be updated. It stays.
  // - "unreachable": the stream cannot reach the server. Reconnecting removes it.
  // - "refresh": fetching the page failed. The next refresh that succeeds removes it.
  // - "reshaped": parts were added to the page or removed from it. A refresh that finds the
  //   same parts again removes it.
  function showNotice(kind, text) {
    let notice = document.getElementById("live-notice-" + kind);
    if (!notice) {
      notice = document.createElement("p");
      notice.id = "live-notice-" + kind;
      notice.className = "notice live-notice";
      document.querySelector("main").prepend(notice);
    }
    notice.textContent = text;
  }

  function clearNotice(kind) {
    const notice = document.getElementById("live-notice-" + kind);
    if (notice) {
      notice.remove();
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      disconnect();
    } else {
      connect();
    }
  });
  connect();
})();
