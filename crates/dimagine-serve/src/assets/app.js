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
//
// No framework, no build step, no network beyond the viewer's own origin.

(function () {
  'use strict';

  // Marks the document so CSS can hide the no-script affordances (the sort
  // submit button) that only exist for a browser without this file.
  document.documentElement.classList.add('js');

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
})();