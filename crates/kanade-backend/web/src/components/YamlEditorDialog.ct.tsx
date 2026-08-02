import { expect, test } from '@playwright/experimental-ct-react';

import {
  CreateDialog,
  CreateScheduleDialog,
  GitManagedDialog,
} from './YamlEditorDialog.ct.harness';

/**
 * Issue #1293: this dialog called `useTranslation` zero times, so opening a
 * manifest from the Japanese console produced an entirely English modal on
 * top of an otherwise Japanese page — title, description, banner and
 * buttons.
 *
 * The locale-catalogue test next door proves the keys EXIST in both
 * languages. It cannot prove the component asks for them, which was the
 * actual defect. These mount the real dialog with the language set and
 * assert on Japanese copy, so deleting a `t()` call fails here even though
 * every key is still present.
 *
 * The read-only branch is covered because the demo stack cannot reach it —
 * its mock never returns a `gitOrigin`, so those five strings would
 * otherwise be translated but never once rendered.
 */

// Assertions run against `page`, not the mounted component: Radix renders
// the dialog through a portal into document.body, so the content is a
// sibling of the mount root rather than a descendant of it.
test.describe('YamlEditorDialog i18n', () => {
  test('the editable branch is Japanese, and the code sample is not', async ({ mount, page }) => {
    await mount(<CreateDialog />);
    await expect(page.getByText('新しいジョブ')).toBeVisible();
    await expect(page.getByText(/スキーマ対応のエディタです/)).toBeVisible();
    await expect(page.getByRole('button', { name: 'キャンセル' })).toBeVisible();
    await expect(page.getByRole('button', { name: '保存' })).toBeVisible();
    // The media type is a wire value, not prose — it stays Latin.
    await expect(page.getByText('application/yaml')).toBeVisible();
  });

  test('the noun follows the kind', async ({ mount, page }) => {
    await mount(<CreateScheduleDialog />);
    await expect(page.getByText('新しいスケジュール')).toBeVisible();
  });

  test('the Git-managed branch is Japanese, but the CLI command is not', async ({ mount, page }) => {
    await mount(<GitManagedDialog />);
    await expect(page.getByText(/読み取り専用/).first()).toBeVisible();
    await expect(page.getByText('Git 管理', { exact: true })).toBeVisible();
    await expect(page.getByRole('link', { name: /リポジトリを開く/ })).toBeVisible();
    await expect(page.getByRole('button', { name: '閉じる' })).toBeVisible();
    // Translating this would hand the operator a command that does not
    // exist — `kanade ジョブ create`.
    await expect(page.getByText('kanade job create')).toBeVisible();
    // Likewise the YAML key that names the source path.
    await expect(page.getByText('manifest:', { exact: true })).toBeVisible();
  });
});
