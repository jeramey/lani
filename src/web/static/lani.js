// LANI: line selection and the new-issue form in the code views (docs/design.md §13.4).
//
// Click a line number to select the line; shift-click another on the same side to select the
// range. The URL hash follows the selection (#L42-L48 in a file, #<file>-H42-H48 or -B… in the
// changes), so a selection can be linked to. The page's <template> elements hold the forms; this
// script only fills them in. It adds no inline code or styles, so it runs under the CSP.
"use strict";

(function () {
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
})();
