(function () {
    'use strict';

    var roleKeys = [
        'keyword',
        'string',
        'number',
        'comment',
        'function',
        'type',
        'variable',
        'operator',
        'punctuation',
        'constant',
    ];
    var roleVariables = {
        keyword: '--raven-c-keyword',
        string: '--raven-c-string',
        number: '--raven-c-number',
        comment: '--raven-c-comment',
        function: '--raven-c-function',
        type: '--raven-c-type',
        variable: '--raven-c-variable',
        operator: '--raven-c-operator',
        punctuation: '--raven-c-punctuation',
        constant: '--raven-c-constant',
    };
    var payloadKeys = [
        '__ravenQuartoTheme',
        'background',
        'enabled',
        'fontMono',
        'fontText',
        'foreground',
        'roles',
    ];
    var roleSchemaKeys = roleKeys.slice().sort();
    var colorPattern = /^(?:#[0-9a-f]{3,4}|#[0-9a-f]{6}(?:[0-9a-f]{2})?|rgb\(\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*\)|rgba\(\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*(?:0|1|0?\.\d+|\d{1,3}%)\s*\))$/i;
    // Keep this character guard aligned with sanitizeFontFamily and the
    // raven.quarto font-setting schema. Structural checks happen upstream.
    var fontPattern = /^[^;{}<>\\\n\r\t\f\v\0]*$/;

    function isRecord(value) {
        return value !== null && typeof value === 'object' && !Array.isArray(value);
    }

    function hasExactKeys(value, expected) {
        var actual = Object.keys(value).sort();
        if (actual.length !== expected.length) return false;
        for (var index = 0; index < expected.length; index += 1) {
            if (actual[index] !== expected[index]) return false;
        }
        return true;
    }

    function isColor(value) {
        return typeof value === 'string' && colorPattern.test(value);
    }

    function isThemeMessage(value) {
        if (!isRecord(value) || !hasExactKeys(value, payloadKeys)) return false;
        if (value.__ravenQuartoTheme !== true || typeof value.enabled !== 'boolean') return false;
        if (!isColor(value.background) || !isColor(value.foreground)) return false;
        if (typeof value.fontText !== 'string' || !fontPattern.test(value.fontText)) return false;
        if (typeof value.fontMono !== 'string' || !fontPattern.test(value.fontMono)) return false;
        if (!isRecord(value.roles) || !hasExactKeys(value.roles, roleSchemaKeys)) return false;
        for (var index = 0; index < roleKeys.length; index += 1) {
            if (!isColor(value.roles[roleKeys[index]])) return false;
        }
        return true;
    }

    function isPingMessage(value) {
        return isRecord(value)
            && hasExactKeys(value, ['type'])
            && value.type === 'raven-quarto-theme-ping';
    }

    function postReady() {
        window.parent.postMessage({ type: 'raven-quarto-theme-ready' }, '*');
    }

    function applyTheme(payload) {
        var root = document.documentElement;
        root.style.setProperty('--raven-bg', payload.background);
        root.style.setProperty('--raven-fg', payload.foreground);
        for (var index = 0; index < roleKeys.length; index += 1) {
            var role = roleKeys[index];
            root.style.setProperty(roleVariables[role], payload.roles[role]);
        }
        root.style.setProperty('--raven-font-text', payload.fontText);
        root.style.setProperty('--raven-font-mono', payload.fontMono);
        root.classList.toggle('raven-vscode-theme', payload.enabled);
    }

    // Install the listener before the first ready handshake: parent delivery
    // can be synchronous with observing that handshake.
    window.addEventListener('message', function (event) {
        try {
            if (event.source !== window.parent) return;
            if (isPingMessage(event.data)) {
                postReady();
                return;
            }
            if (!isThemeMessage(event.data)) return;
            applyTheme(event.data);
            postReady();
        } catch (_error) {
            // A malformed or hostile page message must never escape the bridge.
        }
    });

    postReady();
}());
