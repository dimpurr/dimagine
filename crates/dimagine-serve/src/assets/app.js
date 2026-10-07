// dimagine viewer enhancement script (app.js)
//
// Everything here is an enhancement: the pages work without this file. The
// script only adds what server-rendered HTML cannot do —
//
//   * the size control remembers the last choice on this device
//   * the sort menu submits itself
//   * single click selects a tile and fills the inspector (desktop)
//   * arrows move the selection, Enter opens, Esc clears, / focuses search
//   * "Load more" fetches the next page from /api/view and appends it
//   * recent views, kept on this device only
//   * the theme is a choice on this device: Auto, Light or Dark
//   * the chips inside the search capsule scroll, and the tag list filters
//
// No framework, no build step, no network beyond the viewer's own origin.

(function () {
  'use strict';

  // Marks the document so CSS can hide the no-script affordances (the sort
  // submit button) that only exist for a browser without this file.
  document.documentElement.classList.add('js');

  /* =============================================== W43: theme =========== */

  // W34 audit #11: the palette used to be the operating system's alone. The
  // choice is 'auto' (follow the system), 'light' or 'dark', kept on this
  // device. `applyTheme` runs here, before anything else in this file, so a
  // reload paints the chosen theme rather than flashing the other one.
  var THEME_KEY = 'dimagine.theme';

  function storedTheme() {
    try {
      var value = window.localStorage.getItem(THEME_KEY);
      return value === 'light' || value === 'dark' ? value : 'auto';
    } catch (error) {
      return 'auto';
    }
  }

  function applyTheme(choice) {
    if (choice === 'auto') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.setAttribute('data-theme', choice);
  }

  function rememberTheme(choice) {
    try {
      window.localStorage.setItem(THEME_KEY, choice);
    } catch (error) {
      // A device that refuses storage keeps the choice for this page only.
    }
  }

  var themeChoice = storedTheme();
  applyTheme(themeChoice);

  // The switch is the toolbar's fifth control, and the toolbar is drawn by
  // the server for every page; building it here keeps a theme preference a
  // device-only detail with nothing to ask the server for. Without this
  // file the pages follow the system, exactly as they did before.
  (function buildThemeSwitch() {
    var host =
      document.querySelector('.toolbar-controls') || document.querySelector('.top-bar-row');
    if (!host) return;
    var group = document.createElement('div');
    group.className = 'theme-switch';
    group.setAttribute('role', 'group');
    group.setAttribute('aria-label', 'Theme');
    ['auto', 'light', 'dark'].forEach(function (choice) {
      var button = document.createElement('button');
      button.type = 'button';
      button.dataset.themeChoice = choice;
      button.textContent = choice.charAt(0).toUpperCase() + choice.slice(1);
      button.setAttribute('aria-pressed', String(choice === themeChoice));
      if (choice === themeChoice) button.classList.add('active');
      group.appendChild(button);
    });
    group.addEventListener('click', function (event) {
      var button = event.target.closest ? event.target.closest('button') : null;
      if (!button) return;
      var choice = button.dataset.themeChoice;
      if (!choice) return;
      applyTheme(choice);
      rememberTheme(choice);
      Array.prototype.forEach.call(group.querySelectorAll('button'), function (each) {
        var on = each === button;
        each.classList.toggle('active', on);
        each.setAttribute('aria-pressed', String(on));
      });
    });
    host.appendChild(group);
  })();

  var SIZE_KEY = 'dimagine.size';
  var RECENT_KEY = 'dimagine.recent-views';
  var RECENT_LIMIT = 10;

  var grid = document.querySelector('.grid');
  var inspector = document.querySelector('.right-inspector');

  /* ------------------------------------------------------- size control */

  function applySize(size, persist) {
    if (!grid || !size) return;
    grid.classList.remove('size-s', 'size-m', 'size-l');
    grid.classList.add('size-' + size);
    Array.prototype.forEach.call(
      document.querySelectorAll('.size-toggle-btn'),
      function (button) {
        var on = button.dataset.size === size;
        button.classList.toggle('active', on);
        if (on) button.setAttribute('aria-current', 'true');
        else button.removeAttribute('aria-current');
      }
    );
    if (persist) {
      try {
        window.localStorage.setItem(SIZE_KEY, size);
      } catch (error) {
        // A device that refuses storage still gets the size for this page.
      }
    }
  }

  function rememberedSize() {
    try {
      return window.localStorage.getItem(SIZE_KEY);
    } catch (error) {
      return null;
    }
  }

  function rememberSize(size) {
    try {
      window.localStorage.setItem(SIZE_KEY, size);
    } catch (error) {
      // Not fatal: the links still work without the script.
    }
  }

  Array.prototype.forEach.call(
    document.querySelectorAll('.size-toggle-btn'),
    function (button) {
      button.addEventListener('click', function () {
        var size = button.dataset.size;
        if (!size) return;
        applySize(size, false);
        rememberSize(size);
      });
    }
  );

  var preferred = rememberedSize();
  if (preferred) applySize(preferred, false);

  /* ---------------------------------------------------------- sort menu */

  var sortSelect = document.querySelector('.sort-select');
  if (sortSelect) {
    sortSelect.addEventListener('change', function () {
      var form = sortSelect.form;
      if (form) form.submit();
    });
  }

  /* --------------------------------------------------------- copy button */

  document.addEventListener('click', function (event) {
    var button = event.target.closest ? event.target.closest('.copy-btn') : null;
    if (!button) return;
    var text = button.dataset.copyText;
    if (!text || !navigator.clipboard) return;
    navigator.clipboard.writeText(text).then(
      function () {
        var original = button.textContent;
        button.textContent = 'Copied';
        button.disabled = true;
        window.setTimeout(function () {
          button.textContent = original;
          button.disabled = false;
        }, 1500);
      },
      function () {
        // A denied clipboard permission is not worth an error dialog.
      }
    );
  });

  /* ------------------------------------------------------------ inspector */

  function currentViewQuery() {
    return window.location.search.replace(/^\?/, '');
  }

  function imageHref(path) {
    var view = currentViewQuery();
    return (
      '/image/' +
      encodeURIComponent(path).replace(/%2F/g, '/') +
      (view ? '?v=' + encodeURIComponent(view) : '')
    );
  }

  function fillInspector(path) {
    if (!inspector || !path) return;
    inspector.innerHTML =
      '<p class="inspector-empty">Loading details…</p>';
    fetch('/api/image/' + encodeURIComponent(path).replace(/%2F/g, '/'), {
      credentials: 'same-origin',
    })
      .then(function (response) {
        if (!response.ok) throw new Error('image not readable');
        return response.json();
      })
      .then(function (detail) {
        inspector.innerHTML = inspectorHtml(path, detail);
      })
      .catch(function () {
        inspector.innerHTML =
          '<p class="inspector-empty">Could not read this image.</p>';
      });
  }

  function inspectorHtml(path, detail) {
    var properties = detail.properties || {};
    var title =
      (typeof properties.title === 'string' && properties.title) ||
      path.split('/').pop();
    var tags = Array.isArray(properties.tags) ? properties.tags : [];
    var source =
      typeof properties.source === 'string' ? properties.source : '';
    var href = imageHref(path);
    var parts = [];

    parts.push(
      '<a href="' +
        escapeAttribute(href) +
        '"><img class="inspector-preview-img" src="/thumb/' +
        escapeAttribute(path) +
        '" alt="' +
        escapeAttribute(title) +
        '"></a>'
    );
    parts.push('<h3 class="inspector-title">' + escapeText(title) + '</h3>');
    parts.push(
      '<div class="inspector-path-row"><code class="image-path">' +
        escapeText(path) +
        '</code><button type="button" class="copy-btn" data-copy-text="' +
        escapeAttribute(path) +
        '">Copy</button></div>'
    );
    if (source.indexOf('http') === 0) {
      parts.push(
        '<div class="inspector-section"><h4>Source</h4><a class="source-link" href="' +
          escapeAttribute(source) +
          '" rel="noreferrer noopener">' +
          escapeText(source) +
          '</a></div>'
      );
    }
    if (tags.length) {
      parts.push(
        '<div class="inspector-section"><h4>Tags</h4><div class="inspector-tags">' +
          tags
            .map(function (tag) {
              return (
                '<a class="inspector-tag" href="/?tag=' +
                encodeURIComponent(tag) +
                '">' +
                escapeText(tag) +
                '</a>'
              );
            })
            .join('') +
          '</div></div>'
      );
    }
    if (detail.body_html) {
      // The server renders the note to sanitised HTML with
      // its wikilinks and embeds resolved (FORMAT §5.1),
      // so the inspector shows the rendered note, not the
      // raw Markdown.
      parts.push(
        '<div class="inspector-section"><h4>Note</h4><div class="note-body">' +
          detail.body_html +
          '</div></div>'
      );
    } else if (detail.body) {
      // A detail without rendered HTML still shows the
      // note, as escaped text.
      parts.push(
        '<div class="inspector-section"><h4>Note</h4><div class="note-body">' +
          escapeText(detail.body.slice(0, 400)) +
          '</div></div>'
      );
    }
    if (detail.note_path) {
      parts.push(
        '<div class="inspector-section"><h4>Note file</h4><code class="image-path">' +
          escapeText(detail.note_path) +
          '</code></div>'
      );
    }
    parts.push('<p><a href="' + escapeAttribute(href) + '">Open the image page</a></p>');
    return parts.join('');
  }

  function escapeText(value) {
    return String(value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
  }

  function escapeAttribute(value) {
    return escapeText(value).replace(/"/g, '&quot;');
  }

  /* -------------------------------------------- selection and keyboard */

  var selected = null;

  function selectTile(tile, focus) {
    if (selected) selected.classList.remove('selected');
    selected = tile;
    if (!selected) {
      if (inspector) {
        inspector.innerHTML =
          '<p class="inspector-empty">Select an image to see its details.</p>';
      }
      return;
    }
    selected.classList.add('selected');
    selected.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    fillInspector(selected.dataset.path);
    if (focus) selected.focus();
  }

  function tiles() {
    return Array.prototype.slice.call(document.querySelectorAll('.tile[data-path]'));
  }

  Array.prototype.forEach.call(
    document.querySelectorAll('.tile[data-path]'),
    function (tile) {
      // On a wide screen a click selects and an Enter or double click opens.
      // Without the script the tile is a plain link either way.
      tile.addEventListener('click', function (event) {
        if (window.matchMedia('(min-width: 1200px)').matches) {
          event.preventDefault();
          selectTile(tile, false);
        }
      });
      tile.addEventListener('dblclick', function () {
        window.location.href = imageHref(tile.dataset.path);
      });
    }
  );

  document.addEventListener('keydown', function (event) {
    var target = event.target;
    var typing =
      target &&
      (target.tagName === 'INPUT' ||
        target.tagName === 'TEXTAREA' ||
        target.tagName === 'SELECT');
    if (typing) {
      if (event.key === 'Escape') target.blur();
      return;
    }
    if (event.metaKey || event.ctrlKey || event.altKey) return;

    if (event.key === '/') {
      var field = document.querySelector('.search-pill-container input');
      if (field) {
        event.preventDefault();
        field.focus();
        field.select();
      }
      return;
    }
    if (event.key === 'Escape') {
      selectTile(null);
      return;
    }

    var all = tiles();
    if (!all.length) return;
    var index = selected ? all.indexOf(selected) : -1;

    if (event.key === 'ArrowRight' || event.key === 'ArrowDown') {
      event.preventDefault();
      selectTile(all[(index + 1) % all.length], true);
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') {
      event.preventDefault();
      selectTile(all[(index - 1 + all.length) % all.length], true);
    } else if (event.key === 'Enter' && selected) {
      window.location.href = imageHref(selected.dataset.path);
    }
  });

  /* --------------------------------------------------------- load more */

  var loadMore = document.querySelector('.load-more-btn');
  if (loadMore && grid) {
    loadMore.addEventListener('click', function (event) {
      event.preventDefault();
      var nextPage = parseInt(loadMore.dataset.nextPage || '2', 10);
      var url = new URL(window.location.href);
      url.pathname = '/api/view';
      url.searchParams.set('p', String(nextPage));

      loadMore.textContent = 'Loading…';
      fetch(url.toString(), { credentials: 'same-origin' })
        .then(function (response) {
          if (!response.ok) throw new Error('page not available');
          return response.json();
        })
        .then(function (page) {
          if (!page.items || !page.items.length) {
            loadMore.remove();
            return;
          }
          page.items.forEach(function (item) {
            grid.appendChild(tileElement(item));
          });
          var shown = grid.querySelectorAll('.tile[data-path]').length;
          if (shown >= page.total) {
            loadMore.remove();
          } else {
            loadMore.dataset.nextPage = String(nextPage + 1);
            loadMore.textContent = 'Load more';
          }
        })
        .catch(function () {
          // The link still works: reloading the page is the fallback.
          loadMore.textContent = 'Load more';
          loadMore.removeAttribute('data-next-page');
          window.location.href = window.location.href;
        });
    });
  }

  function tileElement(item) {
    var label = item.title || item.path.split('/').pop();
    var tile = document.createElement('a');
    tile.className = 'tile';
    tile.dataset.path = item.path;
    tile.href = imageHref(item.path);

    var image = document.createElement('img');
    image.loading = 'lazy';
    image.src = '/thumb/' + encodeURIComponent(item.path).replace(/%2F/g, '/');
    image.alt = label;

    var caption = document.createElement('span');
    caption.className = 'tile-caption';
    caption.textContent = label;

    tile.appendChild(image);
    tile.appendChild(caption);
    tile.addEventListener('click', function (clickEvent) {
      if (window.matchMedia('(min-width: 1200px)').matches) {
        clickEvent.preventDefault();
        selectTile(tile, false);
      }
    });
    return tile;
  }

  /* ------------------------------------------------------- recent views */

  var recentList = document.querySelector('.recent-views-list');
  if (recentList) {
    var recent = readRecent();
    recentList.innerHTML = recent.length
      ? recent
          .map(function (entry) {
            return (
              '<li><a href="' +
              escapeAttribute(entry.url) +
              '">' +
              escapeText(entry.label) +
              '</a></li>'
            );
          })
          .join('')
      : '<li>No recent searches yet.</li>';
  }

  function readRecent() {
    try {
      var stored = JSON.parse(window.localStorage.getItem(RECENT_KEY) || '[]');
      return Array.isArray(stored) ? stored : [];
    } catch (error) {
      return [];
    }
  }

  // A search or a tag view is worth remembering; a folder click is not.
  (function recordCurrentView() {
    if (window.location.pathname !== '/') return;
    var params = new URLSearchParams(window.location.search);
    var text = params.get('q');
    var tag = params.get('tag');
    if (!text && !tag) return;
    var label = text ? 'Search: ' + text : 'Tag: ' + tag;
    var url = window.location.pathname + window.location.search;
    var kept = readRecent().filter(function (entry) {
      return entry.url !== url;
    });
    kept.unshift({ label: label, url: url });
    try {
      window.localStorage.setItem(RECENT_KEY, JSON.stringify(kept.slice(0, RECENT_LIMIT)));
    } catch (error) {
      // Storage is optional; the list simply does not persist.
    }
  })();

  /* =================================== W43: chips, tag filter, loading == */

  // §4.3: the chips of the active scope live inside the search capsule and
  // scroll sideways on one line. The row says it is overflowing with a fade
  // at the edge where content is cut off, so the fade appears only when
  // there is something to fade to.
  function syncChipScroller(row) {
    var overflowing = row.scrollWidth > row.clientWidth + 1;
    row.classList.toggle('scrollable', overflowing);
    if (!overflowing) {
      row.classList.remove('at-start', 'at-end');
      return;
    }
    row.classList.toggle('at-start', row.scrollLeft <= 1);
    row.classList.toggle('at-end', row.scrollLeft + row.clientWidth >= row.scrollWidth - 1);
  }

  Array.prototype.forEach.call(document.querySelectorAll('.scope-chips'), function (row) {
    syncChipScroller(row);
    row.addEventListener('scroll', function () {
      syncChipScroller(row);
    });
  });

  window.addEventListener('resize', function () {
    Array.prototype.forEach.call(document.querySelectorAll('.scope-chips'), syncChipScroller);
  });

  // The ✕ is a real link, so dropping a filter works without this file. The
  // script only plays its 120ms exit first, then follows the same href.
  // A keyboard activation goes at once: keyboard-initiated actions never
  // animate (DESIGN.md §2.6).
  document.addEventListener('click', function (event) {
    var remove = event.target.closest ? event.target.closest('.chip-remove') : null;
    if (!remove) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.detail === 0) {
      return;
    }
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    var chip = remove.closest('.chip');
    if (!chip || chip.classList.contains('removing')) return;
    event.preventDefault();
    chip.classList.add('removing');
    window.setTimeout(function () {
      window.location.href = remove.href;
    }, 120);
  });

  // §4.7: the tag list is on the page already — the filter box narrows both
  // groups by hiding the rows that do not match, and the inline state says
  // so when nothing does. Matching ignores case and accents, because a
  // person types "ene" for "écrémeuse".
  var tagFilter = document.getElementById('tag-filter');
  if (tagFilter) {
    var tagGroups = Array.prototype.slice.call(document.querySelectorAll('[data-tag-group]'));
    var noTagMatch = document.querySelector('.inline-state');

    function folded(value) {
      return value.toLowerCase().normalize('NFD').replace(/[\u0300-\u036f]/g, '');
    }

    function filterTags() {
      var needle = folded(tagFilter.value.trim());
      var shown = 0;
      tagGroups.forEach(function (group) {
        var inGroup = 0;
        Array.prototype.forEach.call(group.querySelectorAll('.tag-row'), function (row) {
          var name = row.querySelector('.tag-name');
          var hits = !needle || folded(name ? name.textContent : '').indexOf(needle) !== -1;
          row.hidden = !hits;
          if (hits) inGroup += 1;
        });
        group.hidden = inGroup === 0;
        shown += inGroup;
      });
      if (noTagMatch) noTagMatch.hidden = shown !== 0;
    }

    tagFilter.addEventListener('input', filterTags);
    var clearTagFilter = document.querySelector('[data-clear-tag-filter]');
    if (clearTagFilter) {
      clearTagFilter.addEventListener('click', function () {
        tagFilter.value = '';
        filterTags();
        tagFilter.focus();
      });
    }
  }

  // §4.8 loading: the request for the next page belongs to "Load more"
  // above; this keeps a skeleton in the button's place while it is in
  // flight, and ends it when the tiles arrive. The grid keeps its tiles, so
  // the columns never jump.
  (function skeletonWhilePaging() {
    var button = document.querySelector('.load-more-btn');
    if (!button || !grid) return;
    button.addEventListener('click', function () {
      button.classList.add('is-loading');
    });
    new MutationObserver(function () {
      button.classList.remove('is-loading');
    }).observe(grid, { childList: true });
  })();
})();

/* === SIDEBAR (W42) =========================================== */
/* W42 styling batch (W34 audit #4/#5/#6): the collapsible folder tree,
   the resizable sidebar width, and the styled tooltip on truncated rows.
   All of it is enhancement — without this file the full tree is shown,
   the sidebar keeps its default width, and `title` still carries every
   long name. Kept as its own section so parallel batches merge cleanly. */

(function () {
  'use strict';

  var TREE_KEY = 'dimagine.folder-tree';
  var WIDTH_KEY = 'dimagine.sidebar-w';
  var WIDTH_MIN = 200;
  var WIDTH_MAX = 360;
  var WIDTH_DEFAULT = 248;
  var TIP_DELAY = 400;

  var sidebar = document.querySelector('.desktop-sidebar');

  function readStored(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (error) {
      return null;
    }
  }

  function writeStored(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (error) {
      // Storage is optional; the choice simply does not persist.
    }
  }

  /* -------------------------------------------------- folder tree */

  var tree = sidebar ? sidebar.querySelector('.sidebar-tree') : null;
  if (tree) {
    var parentOf = {};
    var open;
    try {
      open = JSON.parse(readStored(TREE_KEY) || '{}');
    } catch (error) {
      open = {};
    }
    if (!open || typeof open !== 'object') open = {};

    Array.prototype.forEach.call(tree.querySelectorAll('.folder-row'), function (row) {
      parentOf[row.dataset.folder] = row.dataset.parent || '';
    });

    // Top level expanded, deeper levels collapsed, unless the device
    // remembers a choice.
    function isOpen(folder) {
      if (Object.prototype.hasOwnProperty.call(open, folder)) return !!open[folder];
      return !parentOf[folder];
    }

    // The selected folder is never hidden behind a remembered closed
    // ancestor — force the trail open for this view (not persisted until
    // the next toggle writes the map).
    var activeRow = tree.querySelector('.folder-row.active');
    if (activeRow) {
      var ancestor = parentOf[activeRow.dataset.folder];
      while (ancestor) {
        open[ancestor] = true;
        ancestor = parentOf[ancestor];
      }
    }

    function applyTree() {
      Array.prototype.forEach.call(tree.querySelectorAll('.folder-row'), function (row) {
        var hidden = false;
        var parent = parentOf[row.dataset.folder];
        while (parent) {
          if (!isOpen(parent)) {
            hidden = true;
            break;
          }
          parent = parentOf[parent];
        }
        row.parentNode.hidden = hidden;
      });
      Array.prototype.forEach.call(tree.querySelectorAll('.tree-triangle'), function (button) {
        var expanded = isOpen(button.dataset.folder);
        button.setAttribute('aria-expanded', String(expanded));
        button.setAttribute(
          'aria-label',
          (expanded ? 'Collapse ' : 'Expand ') + button.dataset.folder.split('/').pop()
        );
      });
    }

    tree.addEventListener('click', function (event) {
      var button = event.target.closest ? event.target.closest('.tree-triangle') : null;
      if (!button) return;
      event.preventDefault();
      var folder = button.dataset.folder;
      open[folder] = !isOpen(folder);
      writeStored(TREE_KEY, JSON.stringify(open));
      applyTree();
    });

    applyTree();
  }

  /* -------------------------------------------------- width resizing */

  if (sidebar) {
    var resizer = document.createElement('div');
    var dragging = false;

    function setWidth(w, persist) {
      var value = Math.min(WIDTH_MAX, Math.max(WIDTH_MIN, Math.round(w)));
      document.documentElement.style.setProperty('--sidebar-w', value + 'px');
      resizer.setAttribute('aria-valuenow', String(value));
      if (persist) writeStored(WIDTH_KEY, String(value));
    }

    resizer.className = 'sidebar-resizer';
    resizer.setAttribute('role', 'separator');
    resizer.setAttribute('aria-orientation', 'vertical');
    resizer.setAttribute('aria-label', 'Resize sidebar');
    resizer.setAttribute('aria-valuemin', String(WIDTH_MIN));
    resizer.setAttribute('aria-valuemax', String(WIDTH_MAX));
    resizer.tabIndex = 0;
    document.body.appendChild(resizer);

    var remembered = parseInt(readStored(WIDTH_KEY) || '', 10);
    setWidth(isNaN(remembered) ? sidebar.getBoundingClientRect().width : remembered, false);

    resizer.addEventListener('pointerdown', function (event) {
      dragging = true;
      resizer.classList.add('dragging');
      resizer.setPointerCapture(event.pointerId);
      event.preventDefault();
    });
    resizer.addEventListener('pointermove', function (event) {
      if (!dragging) return;
      // The sidebar sits flush with the left edge, so the pointer's x is
      // the width — no measurement, no reflow per move.
      setWidth(event.clientX, false);
    });
    function endDrag() {
      if (!dragging) return;
      dragging = false;
      resizer.classList.remove('dragging');
      setWidth(sidebar.getBoundingClientRect().width, true);
    }
    resizer.addEventListener('pointerup', endDrag);
    resizer.addEventListener('pointercancel', endDrag);
    resizer.addEventListener('dblclick', function () {
      setWidth(WIDTH_DEFAULT, true);
    });
    resizer.addEventListener('keydown', function (event) {
      var current = sidebar.getBoundingClientRect().width;
      if (event.key === 'ArrowLeft') setWidth(current - 8, true);
      else if (event.key === 'ArrowRight') setWidth(current + 8, true);
      else if (event.key === 'Home') setWidth(WIDTH_DEFAULT, true);
      else return;
      event.preventDefault();
    });
  }

  /* -------------------------------------------------- row tooltips */

  // The server gives every row its full name in `title`; this adds the
  // styled tooltip for the rows that actually truncate — 400ms after
  // hover starts, instant on keyboard focus, and instant once another
  // tooltip is already open (DESIGN.md §4.1).
  Array.prototype.forEach.call(
    document.querySelectorAll('.desktop-sidebar .sidebar-row'),
    function (row) {
      var label = row.querySelector('.sidebar-row-label');
      var tip = row.querySelector('.sidebar-tip');
      if (!label || !tip || label.scrollWidth <= label.clientWidth) return;
      var timer = null;

      function show() {
        timer = null;
        tip.classList.add('shown');
      }

      function enter(instant) {
        if (timer || tip.classList.contains('shown')) return;
        if (instant || document.querySelector('.sidebar-tip.shown')) show();
        else timer = window.setTimeout(show, TIP_DELAY);
      }

      function leave() {
        if (timer) {
          window.clearTimeout(timer);
          timer = null;
        }
        tip.classList.remove('shown');
      }

      row.addEventListener('mouseenter', function () {
        enter(false);
      });
      row.addEventListener('mouseleave', leave);
      row.addEventListener('focusin', function () {
        enter(true);
      });
      row.addEventListener('focusout', leave);
    }
  );
})();