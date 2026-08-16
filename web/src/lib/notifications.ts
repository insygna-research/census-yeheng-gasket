import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from '@tauri-apps/plugin-notification';
import { isTauri } from './platform';

/**
 * System-level notification for a finished agent turn.
 * Only fires when the window is not focused — an in-focus user is already
 * watching the stream. Permission is requested lazily on first use.
 */
export async function notifyTurnComplete(title: string, body: string): Promise<void> {
  if (document.hasFocus()) return;

  const snippet = body.replace(/\s+/g, ' ').trim().slice(0, 120) || 'Response complete';

  try {
    if (isTauri) {
      let granted = await isPermissionGranted();
      if (!granted) {
        granted = (await requestPermission()) === 'granted';
      }
      if (granted) {
        sendNotification({ title, body: snippet });
      }
    } else if ('Notification' in window) {
      if (Notification.permission === 'default') {
        await Notification.requestPermission();
      }
      if (Notification.permission === 'granted') {
        new Notification(title, { body: snippet });
      }
    }
  } catch (e) {
    console.warn('Failed to send notification:', e);
  }
}
