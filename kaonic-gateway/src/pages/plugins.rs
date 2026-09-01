use leptos::prelude::*;

use super::PageTitle;

const PLUGINS_JS: &str = r#"
(function() {
    var state = {
        selectedId: '',
        plugins: [],
        installerVersion: '',
        loadError: '',
        loading: false,
        busyAction: {
            pluginId: '',
            action: ''
        },
        modal: {
            open: false,
            pluginId: '',
            kind: 'install',
            file: null,
            uploading: false,
            feedback: '',
            feedbackKind: '',
            dragActive: false
        },
        confirm: {
            open: false,
            pluginId: '',
            action: '',
            title: '',
            message: '',
            confirmLabel: '',
            confirmKind: '',
            busy: false
        }
    };

    function badgeClass(status) {
        var text = String(status || '').trim().toLowerCase();
        if (text.includes('running') || text === 'active') { return 'badge badge-ok'; }
        if (text.includes('stopped') || text.includes('inactive')) { return 'badge badge-warn'; }
        if (text.includes('error') || text.includes('failed')) { return 'badge badge-err'; }
        return 'badge';
    }

    function setInstallerVersion(version) {
        state.installerVersion = version || '';
        var el = document.getElementById('plugins-installer-version');
        if (!el) { return; }
        el.textContent = state.installerVersion
            ? 'v' + state.installerVersion
            : 'Unavailable';
    }

    function showActionError(err) {
        window.alert('Plugin action failed: ' + (err && err.message ? err.message : err));
    }

    function setLoading(loading) {
        state.loading = !!loading;
        var installBtn = document.querySelector('[data-plugin-install]');
        if (installBtn) { installBtn.disabled = state.loading; }
        var updateBtn = document.querySelector('[data-plugin-upload]');
        if (updateBtn) { updateBtn.disabled = state.loading; }
        var installerUpgradeBtn = document.querySelector('[data-installer-upgrade]');
        if (installerUpgradeBtn) { installerUpgradeBtn.disabled = state.loading; }
        var modalChooseBtn = document.querySelector('[data-plugin-modal-choose]');
        if (modalChooseBtn) { modalChooseBtn.disabled = state.modal.uploading; }
    }

    function currentPlugin() {
        return state.plugins.find(function(plugin) {
            return plugin.id === state.selectedId;
        }) || null;
    }

    function detailValue(value) {
        return value == null || value === '' ? '—' : String(value);
    }

    function formatTimestamp(value) {
        if (!value) { return '—'; }
        var date = new Date(Number(value) * 1000);
        if (Number.isNaN(date.getTime())) { return '—'; }
        return date.toLocaleString();
    }

    function escaped(value) {
        return String(value == null ? '' : value)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/\"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    function githubLink(value) {
        if (value == null || String(value).trim() === '') { return '—'; }
        var url = String(value).trim();
        return '<a class=\"plugins-detail-link\" href=\"' + escaped(url) + '\" target=\"_blank\" rel=\"noreferrer\">' + escaped(url) + '</a>';
    }

    function webviewLink(plugin) {
        var url = webviewUrl(plugin);
        return url ? '<a class=\"plugins-detail-link\" href=\"' + escaped(url) + '\" target=\"_blank\" rel=\"noreferrer\">' + escaped(url) + '</a>' : '—';
    }

    function webviewUrl(plugin) {
        var port = plugin && plugin.webview;
        if (port == null || port === '') { return ''; }
        var numericPort = Number(port);
        if (!Number.isInteger(numericPort) || numericPort < 1 || numericPort > 65535) { return ''; }
        var host = window.location.hostname || window.location.host || '';
        if (!host) { return ''; }
        if (host.includes(':') && host.charAt(0) !== '[') {
            host = '[' + host + ']';
        }
        var scheme = plugin && plugin.tls ? 'https://' : 'http://';
        return scheme + host + ':' + numericPort;
    }

    function iconUrl(plugin) {
        if (!plugin || !plugin.icon) { return ''; }
        // updated_at busts the cache when a plugin update ships a new icon.
        return '/api/plugins/' + encodeURIComponent(plugin.id) + '/icon?v='
            + encodeURIComponent(plugin.updated_at || plugin.version || '');
    }

    function iconMarkup(plugin, className) {
        var url = iconUrl(plugin);
        if (!url) {
            return '<span class=\"' + className + ' ' + className + '--empty\" aria-hidden=\"true\">\ud83e\udde9</span>';
        }
        return '<img class=\"' + className + '\" src=\"' + escaped(url) + '\" alt=\"\" loading=\"lazy\">';
    }

    function ensureSelection() {
        if (!state.plugins.length) {
            state.selectedId = '';
            return;
        }
        var exists = state.plugins.some(function(plugin) { return plugin.id === state.selectedId; });
        if (!exists) {
            state.selectedId = state.plugins[0].id;
        }
    }

    function modalActionLabel() {
        if (state.modal.kind === 'installer-upgrade') { return 'Upgrade'; }
        return state.modal.pluginId ? 'Upload update' : 'Install plugin';
    }

    function modalProgressLabel() {
        if (state.modal.kind === 'installer-upgrade') { return 'Upgrading installer…'; }
        return state.modal.pluginId ? 'Uploading update…' : 'Installing plugin…';
    }

    function modalHeading() {
        if (state.modal.kind === 'installer-upgrade') { return 'Upgrade kaonic-installer'; }
        return state.modal.pluginId ? 'Upload plugin update' : 'Install plugin package';
    }

    function modalDescription() {
        if (state.modal.kind === 'installer-upgrade') {
            return 'Updating of kaonic-installer is not safe. Please make sure device is powered during update.';
        }
        return state.modal.pluginId
            ? 'Drop an updated ZIP package here or choose one from your device.'
            : 'Drag and drop a plugin ZIP here or choose one from your device to install it.';
    }

    function modalDropzoneTitle() {
        return state.modal.kind === 'installer-upgrade'
            ? 'Choose replacement installer binary'
            : 'Drag & drop plugin ZIP';
    }

    function modalDropzoneHint() {
        return state.modal.kind === 'installer-upgrade'
            ? 'or use the file picker below'
            : 'or use the file picker below';
    }

    function modalEmptySelectionText() {
        return state.modal.kind === 'installer-upgrade'
            ? 'No replacement binary selected yet.'
            : 'No ZIP package selected yet.';
    }

    function isBusyAction(pluginId, action) {
        return state.busyAction.pluginId === pluginId && state.busyAction.action === action;
    }

    function openConfirmModal(pluginId, action, title, message, confirmLabel, confirmKind) {
        state.confirm.open = true;
        state.confirm.pluginId = pluginId;
        state.confirm.action = action;
        state.confirm.title = title;
        state.confirm.message = message;
        state.confirm.confirmLabel = confirmLabel;
        state.confirm.confirmKind = confirmKind || '';
        state.confirm.busy = false;
        renderConfirmModal();
    }

    function closeConfirmModal(force) {
        if (state.confirm.busy && !force) { return; }
        state.confirm.open = false;
        state.confirm.pluginId = '';
        state.confirm.action = '';
        state.confirm.title = '';
        state.confirm.message = '';
        state.confirm.confirmLabel = '';
        state.confirm.confirmKind = '';
        state.confirm.busy = false;
        renderConfirmModal();
    }

    function formatFileSize(size) {
        if (!size || size < 1024) { return (size || 0) + ' B'; }
        if (size < 1024 * 1024) { return Math.round(size / 102.4) / 10 + ' KB'; }
        return Math.round(size / 104857.6) / 10 + ' MB';
    }

    function formatMemorySize(size) {
        if (size == null || size === '') { return '—'; }
        return formatFileSize(Number(size));
    }

    function setModalFeedback(text, kind) {
        state.modal.feedback = text || '';
        state.modal.feedbackKind = kind || '';
    }

    function setModalFile(file) {
        state.modal.file = file || null;
        state.modal.dragActive = false;
        if (state.modal.file) {
            setModalFeedback('', '');
        }
        renderModal();
    }

    function openUploadModal(pluginId, kind) {
        state.modal.open = true;
        state.modal.pluginId = pluginId || '';
        state.modal.kind = kind || (pluginId ? 'update' : 'install');
        state.modal.file = null;
        state.modal.uploading = false;
        state.modal.dragActive = false;
        setModalFeedback('', '');
        renderModal();
    }

    function closeUploadModal(force) {
        if (state.modal.uploading && !force) { return; }
        state.modal.open = false;
        state.modal.pluginId = '';
        state.modal.kind = 'install';
        state.modal.file = null;
        state.modal.uploading = false;
        state.modal.dragActive = false;
        setModalFeedback('', '');
        var uploadInput = document.getElementById('plugin-package-input');
        if (uploadInput) { uploadInput.value = ''; }
        renderModal();
    }

    function renderConfirmModal() {
        var backdrop = document.getElementById('plugins-confirm-modal');
        var title = document.getElementById('plugins-confirm-title');
        var message = document.getElementById('plugins-confirm-message');
        var submit = document.getElementById('plugins-confirm-submit');
        var body = document.body;
        if (!backdrop || !title || !message || !submit || !body) { return; }

        backdrop.hidden = !state.confirm.open;
        if (state.modal.open || state.confirm.open) { body.classList.add('modal-open'); }
        else { body.classList.remove('modal-open'); }

        title.textContent = state.confirm.title || 'Confirm action';
        message.textContent = state.confirm.message || '';
        submit.className = 'btn-secondary plugins-confirm-submit'
            + (state.confirm.confirmKind ? ' ' + state.confirm.confirmKind : '');
        submit.disabled = state.confirm.busy;
        submit.innerHTML = state.confirm.busy
            ? '<span class=\"plugins-inline-spinner\" aria-hidden=\"true\"></span>Working…'
            : escaped(state.confirm.confirmLabel || 'Confirm');
    }

    function renderModal() {
        var backdrop = document.getElementById('plugins-upload-modal');
        var title = document.getElementById('plugins-upload-title');
        var description = document.getElementById('plugins-upload-description');
        var warning = document.getElementById('plugins-upload-warning');
        var dropzone = document.getElementById('plugins-upload-dropzone');
        var dropzoneTitle = document.getElementById('plugins-upload-dropzone-title');
        var dropzoneHint = document.getElementById('plugins-upload-dropzone-hint');
        var selected = document.getElementById('plugins-upload-selected');
        var feedback = document.getElementById('plugins-upload-feedback');
        var submit = document.getElementById('plugins-upload-submit');
        var choose = document.getElementById('plugins-upload-choose');
        var uploadInput = document.getElementById('plugin-package-input');
        var body = document.body;
        if (!backdrop || !title || !description || !warning || !dropzone || !dropzoneTitle || !dropzoneHint || !selected || !feedback || !submit || !choose || !uploadInput || !body) { return; }

        backdrop.hidden = !state.modal.open;
        if (state.modal.open || state.confirm.open) { body.classList.add('modal-open'); }
        else { body.classList.remove('modal-open'); }

        title.textContent = modalHeading();
        description.textContent = modalDescription();
        warning.hidden = state.modal.kind !== 'installer-upgrade';
        dropzone.className = 'plugins-upload-dropzone'
            + (state.modal.dragActive ? ' plugins-upload-dropzone--drag' : '')
            + (state.modal.file ? ' plugins-upload-dropzone--ready' : '');
        dropzoneTitle.textContent = modalDropzoneTitle();
        dropzoneHint.textContent = modalDropzoneHint();
        uploadInput.accept = state.modal.kind === 'installer-upgrade' ? '' : '.zip,application/zip';

        if (state.modal.file) {
            selected.textContent = state.modal.file.name + ' • ' + formatFileSize(state.modal.file.size);
            selected.className = 'plugins-upload-selected plugins-upload-selected--ready';
        } else {
            selected.textContent = modalEmptySelectionText();
            selected.className = 'plugins-upload-selected';
        }

        feedback.textContent = state.modal.feedback || '';
        feedback.className = 'plugins-upload-feedback' + (state.modal.feedbackKind ? ' ' + state.modal.feedbackKind : '');

        choose.disabled = state.modal.uploading;
        submit.disabled = state.modal.uploading || !state.modal.file;
        submit.innerHTML = state.modal.uploading
            ? '<span class=\"plugins-inline-spinner\" aria-hidden=\"true\"></span>' + escaped(modalProgressLabel())
            : escaped(modalActionLabel());
    }

    function loadInstallerVersion() {
        return fetch('/api/plugins/installer-version')
            .then(function(resp) {
                if (!resp.ok) {
                    return resp.json().catch(function() { return {}; }).then(function(data) {
                        throw new Error(data.detail || ('HTTP ' + resp.status));
                    });
                }
                return resp.json();
            })
            .then(function(data) {
                setInstallerVersion((data && data.version) || '');
            })
            .catch(function() {
                setInstallerVersion('');
            });
    }

    function loadPlugins(message, kind, options) {
        options = options || {};
        if (!options.background) { setLoading(true); }
        return fetch('/api/plugins')
            .then(function(resp) {
                if (!resp.ok) {
                    return resp.json().catch(function() { return {}; }).then(function(data) {
                        throw new Error(data.detail || ('HTTP ' + resp.status));
                    });
                }
                return resp.json();
            })
            .then(function(plugins) {
                state.plugins = Array.isArray(plugins) ? plugins : [];
                state.loadError = '';
                ensureSelection();
                render();
            })
            .catch(function(err) {
                if (!options.background && !state.plugins.length) {
                    state.loadError = 'Unable to load plugins: ' + (err.message || err);
                    render();
                }
                if (options.propagateError) { throw err; }
            })
            .finally(function() {
                if (!options.background) { setLoading(false); }
            });
    }

    function renderList() {
        var list = document.getElementById('plugins-list');
        var count = document.getElementById('plugins-count');
        if (count) { count.textContent = String(state.plugins.length); }
        if (!list) { return; }
        if (state.loadError && !state.plugins.length) {
            list.innerHTML = '<div class=\"plugins-empty\">' + escaped(state.loadError) + '</div>';
            return;
        }
        if (!state.plugins.length) {
            list.innerHTML = '<div class=\"plugins-empty\">No plugins installed.</div>';
            return;
        }
        list.innerHTML = state.plugins.map(function(plugin) {
            var active = plugin.id === state.selectedId ? ' plugins-list-item--active' : '';
            var kind = plugin.removable ? 'Plugin' : 'System';
            return '<button type=\"button\" class=\"plugins-list-item' + active + '\" data-plugin-select=\"' + plugin.id + '\">'
                + iconMarkup(plugin, 'plugins-list-icon')
                + '<span class=\"plugins-list-text\">'
                    + '<span class=\"plugins-list-name\">' + plugin.name + '</span>'
                    + '<span class=\"plugins-list-meta\">' + kind + ' • v' + plugin.version + '</span>'
                    + '<span class=\"' + badgeClass(plugin.status) + '\">' + plugin.status + '</span>'
                + '</span>'
                + '</button>';
        }).join('');
    }

    function renderDetails() {
        var panel = document.getElementById('plugins-detail');
        if (!panel) { return; }
        var plugin = currentPlugin();
        if (!plugin) {
            panel.innerHTML = state.loadError && !state.plugins.length
                ? '<div class=\"plugins-empty plugins-empty--detail\">' + escaped(state.loadError) + '</div>'
                : '<div class=\"plugins-empty plugins-empty--detail\">Select a plugin from the left list.</div>';
            return;
        }
        var running = String(plugin.status || '').toLowerCase().includes('running') || String(plugin.status || '').toLowerCase() === 'active';
        var restartBusy = isBusyAction(plugin.id, 'restart');
        var deleteBusy = isBusyAction(plugin.id, 'delete');
        var actionLocked = !!state.busyAction.pluginId;
        var appUrl = webviewUrl(plugin);
        var openAppAction = appUrl
            ? '<a class=\"btn-primary\" href=\"' + escaped(appUrl) + '\" target=\"_blank\" rel=\"noreferrer\">Open app</a>'
            : '';
        var updateAction = '<button type=\"button\" class=\"btn-secondary\" data-plugin-upload' + (actionLocked ? ' disabled' : '') + '>Update</button>';
        var toggleAction = '<button type=\"button\" class=\"btn-secondary\" data-plugin-toggle' + (actionLocked ? ' disabled' : '') + '>' + (running ? 'Stop' : 'Start') + '</button>';
        var restartAction = '<button type=\"button\" class=\"btn-secondary plugins-action-btn' + (restartBusy ? ' plugins-action-btn--busy' : '') + '\" data-plugin-restart' + (actionLocked ? ' disabled' : '') + '>'
            + (restartBusy ? '<span class=\"plugins-inline-spinner\" aria-hidden=\"true\"></span>Restarting…' : 'Restart')
            + '</button>';
        var deleteDivider = plugin.removable
            ? '<span aria-hidden=\"true\" style=\"width:1px;height:2rem;background:rgba(148,163,184,0.28);display:inline-block;\"></span>'
            : '';
        var deleteAction = plugin.removable
            ? '<button type=\"button\" class=\"btn-secondary plugins-delete-btn\" data-plugin-delete' + (actionLocked ? ' disabled' : '') + '>'
                + (deleteBusy ? '<span class=\"plugins-inline-spinner\" aria-hidden=\"true\"></span>Deleting…' : 'Delete')
                + '</button>'
            : '';
        panel.innerHTML = ''
            + '<div class=\"card-header\">'
                + iconMarkup(plugin, 'plugins-detail-icon')
                + '<div>'
                    + '<span class=\"card-title\">Plugin Details</span>'
                    + '<h2 class=\"plugins-detail-name\">' + escaped(plugin.name) + '</h2>'
                    + '<div class=\"plugins-detail-developer\">' + escaped(detailValue(plugin.developer)) + '</div>'
                + '</div>'
                + '<div class=\"plugins-detail-badges\">'
                    + (plugin.removable ? '<span class=\"badge\">Plugin</span>' : '<span class=\"badge badge-ok\">System</span>')
                    + (plugin.official ? '<span class=\"badge badge-ok\">Official</span>' : '<span class=\"badge\">Community</span>')
                    + '<span class=\"' + badgeClass(plugin.status) + '\" id=\"plugins-service-status\">' + escaped(plugin.status) + '</span>'
                + '</div>'
            + '</div>'
            + '<p class=\"card-body-text plugins-detail-summary\">' + escaped(plugin.description) + '</p>'
            + '<div class=\"plugins-detail-actions\">'
                + openAppAction
                + updateAction
                + '<span aria-hidden=\"true\" style=\"width:0.6rem;display:inline-block;\"></span>'
                + toggleAction
                + restartAction
                + deleteDivider
                + deleteAction
            + '</div>'
            + '<div class=\"plugins-detail-grid\">'
                 + '<div class=\"info-row\"><span class=\"info-label\">Version</span><span class=\"info-value\">' + escaped(detailValue(plugin.version)) + '</span></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">Channel</span><span class=\"info-value\">' + escaped(detailValue(plugin.channel)) + '</span></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">Service name</span><code class=\"info-value\">' + escaped(detailValue(plugin.service)) + '</code></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">SHA-256</span><code class=\"info-value\">' + escaped(detailValue(plugin.sha256)) + '</code></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">GitHub URL</span><span class=\"info-value\">' + githubLink(plugin.github_url) + '</span></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">Webview</span><span class=\"info-value\">' + webviewLink(plugin) + '</span></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">Icon</span><code class=\"info-value\">' + escaped(detailValue(plugin.icon)) + '</code></div>'
                 + '<div class=\"info-row\"><span class=\"info-label\">TLS</span><span class=\"info-value\">' + (plugin.tls ? 'Enabled' : 'Disabled') + '</span></div>'
              + '</div>';
        if (plugin.systemd_status) {
            panel.innerHTML += ''
                + '<div class=\"plugins-runtime-card\">'
                    + '<div class=\"card-header\">'
                        + '<span class=\"card-title\">Live Service State</span>'
                        + '<span class=\"badge\">Auto refresh 15s</span>'
                    + '</div>'
                    + '<div class=\"plugins-runtime-grid\">'
                        + '<div class=\"info-row\"><span class=\"info-label\">Active state</span><span class=\"info-value\">' + escaped(detailValue(plugin.systemd_status.active_state)) + '</span></div>'
                        + '<div class=\"info-row\"><span class=\"info-label\">Sub-state</span><span class=\"info-value\">' + escaped(detailValue(plugin.systemd_status.sub_state)) + '</span></div>'
                        + '<div class=\"info-row\"><span class=\"info-label\">Unit file</span><span class=\"info-value\">' + escaped(detailValue(plugin.systemd_status.unit_file_state)) + '</span></div>'
                        + '<div class=\"info-row\"><span class=\"info-label\">Main PID</span><span class=\"info-value\">' + escaped(detailValue(plugin.systemd_status.main_pid)) + '</span></div>'
                        + '<div class=\"info-row\"><span class=\"info-label\">Tasks</span><span class=\"info-value\">' + escaped(detailValue(plugin.systemd_status.tasks_current)) + '</span></div>'
                        + '<div class=\"info-row\"><span class=\"info-label\">Memory</span><span class=\"info-value\">' + escaped(formatMemorySize(plugin.systemd_status.memory_current)) + '</span></div>'
                    + '</div>'
                + '</div>';
        }
    }

    function render() {
        renderList();
        renderDetails();
        renderModal();
        renderConfirmModal();
    }

    function actionRequest(url, successKind, successMessage) {
        fetch(url, { method: 'POST' })
            .then(function(resp) {
                return resp.json().catch(function() { return {}; }).then(function(data) {
                    if (!resp.ok) {
                        throw new Error(data.detail || ('HTTP ' + resp.status));
                    }
                    return data;
                });
            })
            .then(function(data) {
                return loadPlugins((data && data.detail) || successMessage, successKind || 'ok', {
                    propagateError: true
                });
            })
            .catch(function(err) {
                showActionError(err);
            });
    }

    function performPluginAction(plugin, action) {
        var method = action === 'delete' ? 'DELETE' : 'POST';
        var url = action === 'delete'
            ? '/api/plugins/' + encodeURIComponent(plugin.id)
            : '/api/plugins/' + encodeURIComponent(plugin.id) + '/' + action;
        var successKind = action === 'delete' ? 'warn' : 'ok';
        var fallbackMessage = action === 'restart'
            ? (plugin.name + ' restarted.')
            : (plugin.name + ' removed.');

        state.busyAction.pluginId = plugin.id;
        state.busyAction.action = action;
        state.confirm.busy = false;
        closeConfirmModal(true);
        render();

        fetch(url, { method: method })
            .then(function(resp) {
                return resp.json().catch(function() { return {}; }).then(function(data) {
                    if (!resp.ok) {
                        throw new Error(data.detail || ('HTTP ' + resp.status));
                    }
                    return data;
                });
            })
            .then(function(data) {
                return loadPlugins((data && data.detail) || fallbackMessage, successKind, {
                    propagateError: true
                });
            })
            .catch(function(err) {
                showActionError(err);
            })
            .finally(function() {
                state.busyAction.pluginId = '';
                state.busyAction.action = '';
                render();
            });
    }

    function submitModalUpload() {
        if (!state.modal.file || state.modal.uploading) { return; }
        var pluginId = state.modal.pluginId;
        var form = new FormData();
        var requestUrl = pluginId ? ('/api/plugins/' + encodeURIComponent(pluginId) + '/upload') : '/api/plugins/install';
        var requestBody = form;
        var requestHeaders = {};
        if (state.modal.kind === 'installer-upgrade') {
            requestUrl = '/api/plugins/kaonic-installer/upgrade';
            requestBody = state.modal.file;
            requestHeaders['Content-Type'] = 'application/octet-stream';
            requestHeaders['X-Kaonic-Filename'] = state.modal.file.name || 'kaonic-installer';
        } else {
            form.append('file', state.modal.file);
        }
        state.modal.uploading = true;
        setModalFeedback((state.modal.kind === 'installer-upgrade'
            ? 'Uploading replacement binary '
            : ((pluginId ? 'Uploading update ' : 'Installing '))) + state.modal.file.name + '…', 'ok');
        setLoading(true);
        renderModal();

        fetch(requestUrl, {
            method: 'POST',
            headers: requestHeaders,
            body: requestBody
        })
            .then(function(resp) {
                return resp.json().catch(function() { return {}; }).then(function(data) {
                    if (!resp.ok) {
                        throw new Error(data.detail || ('HTTP ' + resp.status));
                    }
                    return data;
                });
            })
            .then(function(data) {
                var detail = (data && data.detail) || (state.modal.kind === 'installer-upgrade'
                    ? 'kaonic-installer upgraded.'
                    : (pluginId ? 'Plugin updated.' : 'Plugin installed.'));
                state.modal.uploading = false;
                setModalFeedback(detail, 'ok');
                renderModal();
                return loadPlugins(detail, 'ok').then(function() {
                    window.setTimeout(function() { closeUploadModal(true); }, 900);
                });
            })
            .catch(function(err) {
                state.modal.uploading = false;
                setLoading(false);
                setModalFeedback('Error: ' + (err.message || err), 'warn');
                renderModal();
            });
    }

    document.addEventListener('click', function(ev) {
        var target = ev.target;
        if (!(target instanceof HTMLElement)) { return; }

        var selectBtn = target.closest('[data-plugin-select]');
        if (selectBtn) {
            state.selectedId = selectBtn.getAttribute('data-plugin-select') || '';
            render();
            return;
        }

        if (target.closest('[data-plugin-install]')) {
            openUploadModal('', 'install');
            return;
        }

        if (target.closest('[data-installer-upgrade]')) {
            openUploadModal('', 'installer-upgrade');
            return;
        }

        if (target.closest('[data-plugin-modal-close]') || target.id === 'plugins-upload-modal') {
            closeUploadModal(false);
            return;
        }

        if (target.closest('[data-plugin-confirm-close]') || target.id === 'plugins-confirm-modal') {
            closeConfirmModal(false);
            return;
        }

        if (target.closest('[data-plugin-modal-choose]')) {
            var picker = document.getElementById('plugin-package-input');
            if (picker && !state.modal.uploading) { picker.click(); }
            return;
        }

        if (target.closest('[data-plugin-modal-submit]')) {
            submitModalUpload();
            return;
        }

        if (target.closest('[data-plugin-confirm-submit]')) {
            var confirmPlugin = state.plugins.find(function(item) { return item.id === state.confirm.pluginId; }) || null;
            if (!confirmPlugin || state.confirm.busy) { return; }
            state.confirm.busy = true;
            renderConfirmModal();
            performPluginAction(confirmPlugin, state.confirm.action);
            return;
        }

        var plugin = currentPlugin();
        if (!plugin) { return; }

        if (target.closest('[data-plugin-toggle]')) {
            actionRequest(
                '/api/plugins/' + encodeURIComponent(plugin.id) + '/' + ((String(plugin.status || '').toLowerCase().includes('running') || String(plugin.status || '').toLowerCase() === 'active') ? 'stop' : 'start'),
                'ok',
                plugin.name + ' updated.'
            );
            return;
        }

        if (target.closest('[data-plugin-restart]')) {
            openConfirmModal(
                plugin.id,
                'restart',
                'Restart plugin?',
                'This will restart ' + plugin.name + ' and briefly interrupt the service.',
                'Restart now',
                ''
            );
            return;
        }

        if (target.closest('[data-plugin-upload]')) {
            openUploadModal(plugin.id, 'update');
            return;
        }

        if (target.closest('[data-plugin-delete]')) {
            openConfirmModal(
                plugin.id,
                'delete',
                'Delete plugin?',
                'This will remove ' + plugin.name + ' from the system and stop its service.',
                'Delete plugin',
                'plugins-confirm-submit--danger'
            );
        }
    });

    var uploadInput = document.getElementById('plugin-package-input');
    if (uploadInput) {
        uploadInput.addEventListener('change', function() {
            var file = uploadInput.files && uploadInput.files[0];
            if (!file) { return; }
            setModalFile(file);
        });
    }

    var dropzone = document.getElementById('plugins-upload-dropzone');
    if (dropzone) {
        dropzone.addEventListener('click', function() {
            var picker = document.getElementById('plugin-package-input');
            if (picker && !state.modal.uploading) { picker.click(); }
        });
        ['dragenter', 'dragover'].forEach(function(eventName) {
            dropzone.addEventListener(eventName, function(ev) {
                ev.preventDefault();
                if (state.modal.uploading) { return; }
                state.modal.dragActive = true;
                renderModal();
            });
        });
        ['dragleave', 'dragend'].forEach(function(eventName) {
            dropzone.addEventListener(eventName, function(ev) {
                ev.preventDefault();
                state.modal.dragActive = false;
                renderModal();
            });
        });
        dropzone.addEventListener('drop', function(ev) {
            ev.preventDefault();
            if (state.modal.uploading) { return; }
            state.modal.dragActive = false;
            var files = ev.dataTransfer && ev.dataTransfer.files;
            if (files && files[0]) {
                setModalFile(files[0]);
            } else {
                renderModal();
            }
        });
    }

    document.addEventListener('keydown', function(ev) {
        if (ev.key === 'Escape' && state.confirm.open) {
            closeConfirmModal(false);
        } else if (ev.key === 'Escape' && state.modal.open) {
            closeUploadModal(false);
        }
    });

    window.setInterval(function() {
        if (state.modal.open || state.confirm.open || state.modal.uploading || state.busyAction.pluginId) {
            return;
        }
        loadPlugins('', '', { background: true });
    }, 15000);

    loadInstallerVersion();
    loadPlugins();
})();
"#;

#[component]
pub fn PluginsPage() -> impl IntoView {
    view! {
        <div class="page">
            <div class="page-header">
                <div style="display:flex;align-items:center;gap:12px;flex-wrap:wrap;">
                    <PageTitle icon="🧩" title="Plugins" />
                    <span class="badge">"Prototype"</span>
                </div>
                <button type="button" class="btn-primary" data-plugin-install>"Install"</button>
            </div>

            <div class="card plugins-intro-card">
                <div class="card-header">
                    <span class="card-title">"Plugin Manager"</span>
                </div>
                <p class="card-body-text">
                    "Create your own plugin as a ZIP package with kaonic-plugin.toml, a systemd service file, the plugin binary, and an optional files/ folder for extra runtime assets. The binary is installed into the plugin current directory, files/ is copied into that same runtime folder, and updates replace the full plugin contents with the new package. Ship an icon as files/icon.png and declare it with icon = \"icon.png\" in the manifest — 128x128 PNG or SVG."
                </p>
            </div>

            <div class="card plugins-shell">
                <div class="plugins-layout">
                    <PluginsList />
                    <PluginsDetail />
                </div>
                <div class="plugins-shell-footer">
                    <span class="plugins-shell-footer-label">"Installer version"</span>
                    <span id="plugins-installer-version" class="badge">"Loading…"</span>
                    <button type="button" class="btn-secondary" data-installer-upgrade>"Upgrade installer"</button>
                </div>
            </div>

            <input id="plugin-package-input" class="plugins-file-input" type="file" accept=".zip,application/zip" />
            <div class="modal-backdrop" id="plugins-upload-modal" hidden>
                <div class="modal-card plugins-upload-modal" role="dialog" aria-modal="true" aria-labelledby="plugins-upload-title">
                    <div class="modal-header">
                        <div>
                            <div class="modal-title" id="plugins-upload-title">"Install plugin package"</div>
                        </div>
                        <button type="button" class="modal-close" data-plugin-modal-close aria-label="Close">"×"</button>
                    </div>
                    <div class="plugins-upload-copy">
                        <p class="card-body-text" id="plugins-upload-description">
                            "Drag and drop a plugin ZIP here or choose one from your device to install it."
                        </p>
                        <div class="info-row" id="plugins-upload-warning" hidden>
                            <span class="info-label">"Warning"</span>
                            <span class="info-value">"Updating of kaonic-installer is not safe. Please make sure device is powered during update."</span>
                        </div>
                    </div>
                    <button type="button" class="plugins-upload-dropzone" id="plugins-upload-dropzone">
                        <span class="plugins-upload-icon">"⬆"</span>
                        <span class="plugins-upload-title" id="plugins-upload-dropzone-title">"Drag & drop plugin ZIP"</span>
                        <span class="plugins-upload-hint" id="plugins-upload-dropzone-hint">"or use the file picker below"</span>
                    </button>
                    <div class="plugins-upload-selected" id="plugins-upload-selected">"No ZIP package selected yet."</div>
                    <div class="plugins-upload-feedback" id="plugins-upload-feedback"></div>
                    <div class="modal-actions plugins-upload-actions">
                        <button type="button" class="btn-secondary" data-plugin-modal-close>"Cancel"</button>
                        <button type="button" class="btn-secondary" id="plugins-upload-choose" data-plugin-modal-choose>"Choose file"</button>
                        <button type="button" class="btn-primary" id="plugins-upload-submit" data-plugin-modal-submit>"Install plugin"</button>
                    </div>
                </div>
            </div>
            <div class="modal-backdrop" id="plugins-confirm-modal" hidden>
                <div class="modal-card plugins-confirm-modal" role="dialog" aria-modal="true" aria-labelledby="plugins-confirm-title">
                    <div class="modal-header">
                        <div class="modal-title" id="plugins-confirm-title">"Confirm action"</div>
                        <button type="button" class="modal-close" data-plugin-confirm-close aria-label="Close">"×"</button>
                    </div>
                    <p class="card-body-text plugins-confirm-message" id="plugins-confirm-message">
                        "Are you sure you want to continue?"
                    </p>
                    <div class="modal-actions">
                        <button type="button" class="btn-secondary" data-plugin-confirm-close>"Cancel"</button>
                        <button type="button" class="btn-secondary plugins-confirm-submit" id="plugins-confirm-submit" data-plugin-confirm-submit>"Confirm"</button>
                    </div>
                </div>
            </div>

            <script>{PLUGINS_JS}</script>
        </div>
    }
}

#[component]
fn PluginsList() -> impl IntoView {
    view! {
        <aside class="plugins-sidebar">
            <div class="card-header">
                <span class="card-title">"Installed Plugins"</span>
                <span class="badge" id="plugins-count">"0"</span>
            </div>
            <div class="plugins-list" id="plugins-list"><div class="plugins-empty">"Loading plugins…"</div></div>
        </aside>
    }
}

#[component]
fn PluginsDetail() -> impl IntoView {
    view! {
        <section class="plugins-detail" id="plugins-detail">
            <div class="plugins-empty plugins-empty--detail">
                "Select a plugin from the left list or install a new ZIP package."
            </div>
        </section>
    }
}
