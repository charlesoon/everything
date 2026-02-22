import '@fontsource/inter/latin-400.css';
import '@fontsource/inter/latin-500.css';
import '@fontsource/inter/latin-600.css';
import '@fontsource/jetbrains-mono/latin-400.css';
import { mount } from 'svelte';
import { invoke } from '@tauri-apps/api/core';
import App from './App.svelte';

const bootStartMs =
  (typeof window !== 'undefined' && typeof window.__bootStartMs === 'number')
    ? window.__bootStartMs
    : performance.now();
const bootMs = () => Math.round(performance.now() - bootStartMs);

function bootLog(message, retry = 0) {
  void invoke('frontend_log', {
    msg: `[startup/fe-main] +${bootMs()}ms ${message}`
  }).catch(() => {
    if (retry < 5) {
      setTimeout(() => {
        bootLog(message, retry + 1);
      }, 100);
    }
  });
}

bootLog(`main.js module start (readyState=${document.readyState})`);

const app = mount(App, {
  target: document.getElementById('app')
});

bootLog('App constructed');

export default app;
