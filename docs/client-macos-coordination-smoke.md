# macOS Client pkg・lifecycle の使い捨て環境受入手順

この手順は未実施。対象は macOS 14 以降の Apple Silicon の使い捨て VM のみ。
実機 operator の Client/Agent、receipt database、鍵、queue、question を対象にしない。
paid signing/notarization、Windows/Linux installer、本番 typed dispatch は範囲外。

## 準備と記録

1. clean VM の snapshot を取得する。VM 専用の管理者と二つの標準 GUI user を作り、
   fast user switching と lock/logout を試せる状態にする。SSH 用 session も別に用意する。
   NATS/backend は不要。既存の operator credentials や home を VM にコピーしない。
2. 検証する commit を固定し `git rev-parse HEAD`、OS/arch、二つの pkg の version/digest、
   全 command の終了コードを VM 内の記録に残す。GUI/session/UID/PID 世代は匿名化した
   テスト記録にする。画面の表示/非表示と導入結果を別々に記録する。
3. `sh scripts/build-client-macos-pkg.sh` で参加 protocol を持つ旧版・新版を作る。
   pkg は app だけであり、login LaunchAgent を含まない。ad-hoc 署名の検証は配布信頼ではない。
   Gatekeeper の全体無効化を行わず、VM 内で対象 app だけを必要に応じて承認する。
4. 本番 deploy CLI/remote typed dispatch は無効のまま。handler 試験には VM 内限定の
   ローカル Rust harness を用意し、`NativeSystem::managed_host()` と
   `client_pkg::transaction::Installer` を直接呼ぶ。`Request` の typed operation、
   `ExecContext`、別途承認した immutable plan、署名済み manifest、host、target を固定し、
   VM 専用の一時 publisher/trust root と `Authorization::current` を接続する。
   rollback 用旧版は独立した現在の承認を用意する。既存の `client_pkg::tests` の署名・
   plan fixture が入力の例になる。これは配布済み command ではなく、受入用 harness の
   準備要件である。未準備なら native handler smoke は「未実施」とし、汎用 shell を
   typed deployment の代用にしない。

## 旧 Agent と payload-only pkg の共存

clean snapshot の分岐で、旧 Agent を更新・再起動せずに手動 pkg 導入を試す。
この分岐には handler の管理 receipt を作らず、後の更新試験へ持ち込まない。

```sh
VM_CLIENT_PKG=$(python3 -c 'import json; print("target/client-macos-pkg/" + json.load(open("target/client-macos-pkg/build-record.json"))["artifact_key"])')
sudo /usr/sbin/installer -pkg "$VM_CLIENT_PKG" -target /
```

pkg 出力名は build record の実際の名前を使う。導入前に machine gate が存在しないことを
確認し、標準 GUI user から Finder で Client を開く。手動起動できることと、
root Agent の再起動を要求しないことを確認する。試験のため gate を削除して作り直すことは
しない。初回 provisioning と競合する起動は下記の handler 分岐と isolated flock 試験で確認する。
この分岐が終わったら clean snapshot に戻す。

## handler の新規導入・更新・no-op・rollback

1. bundle、OS receipt、managed receipt が全て無い snapshot で harness の旧版 install を実行。
   `installed_verified` と GUI `not_launched` を別々に確認する。初回導入から未稼働 session を
   勝手に起動してはならない。root:wheel、mode/ACL、bundle version、pkgutil receipt を確認する。
2. 標準 GUI user A で Client を開き、helper の存在と同 UID/session・親子関係を記録する。
   user B に切り替えて同様に開く。helper 起動後に VM を一度 lock/unlock し、
   unlock 通知と現在の lock snapshot の両方が得られた B で新版更新を実行する。
   snapshot が得られない OS では unknown として復帰ゼロを期待し、対応を未確認と記録する。
   closure 保存後は新規/second-instance/deep-link/既存 emergency fallback の表示が抑止されること、
   A/B の検証済み instance だけが停止すること、非 active の A が復帰しないことを確認する。
   B の復帰 argv は `--tray`、window は非表示であることを確認する。
3. 同じ新版を再実行し `no_op_verified`、installer 呼出し無し、GUI 停止/再起動無しを確認する。
4. harness の明示的な検証失敗注入で、quiescent な installer 成功後に rollback を要求する。
   独立した旧版承認・trust 再検証を通り、OS installer で旧版が復元されることを確認する。
   部分 bundle や unknown worker を raw file copy で補修して成功扱いにしない。
5. 各更新を別 snapshot で繰り返す。A/B の明示 Quit 済み、更新中の Quit、永続 opt-out、
   lock、logout、SSH/headless、helper 失効/応答遅延、legacy/未登録 Client をそれぞれ試す。
   該当 session の自動復帰はゼロ、未登録 instance がある場合は停止自体がゼロであることを確認する。
   Quit と opt-out の marker は独立して保持され、手動起動だけが当該 session の Quit を解除する。
   discovery 中断後も GUI が残るケースでは、helper 切断後の明示 Quit が完了することを確認する。
6. graceful timeout/escalation や PID 再利用の native 試験は VM 内の test-owned synthetic app
   だけで行う。basename が同じ別 executable、変わった inode/token、別 UID/session、
   任意 caller PID を signal 対象にしないことを記録する。偽 adapter の結果で代用した項目は
   「simulated」と記録し、native signal を実行したとは記載しない。

## crash・復旧・初回 gate 作成競合

harness に transaction checkpoint ごとの停止を用意し、別 snapshot で prepared、installing
（mutation intent 前/後）、installer returned、rollback、terminal journal の各境界を中断する。

- Client 起動を初回 gate 作成と重ねる。移行用 `/Library` 共有 flock を持つ Client が残る間、
  writer は mutation intent を記録できないことを確認する。root の umask 077 でも新規 gate
  directory が 0755、lease が 0644 となり、標準 user が起動可能であることを確認する。
- mutation 前に承認を失効させ、installer 呼出しゼロ、旧状態一致、abandoned、gate 解放を確認する。
  root coordinator 終了後にも同じ transaction の recover で解放できることを確認する。
- mutation 後の中断は gate が残ることを確認する。installer worker/quiescence と bundle/receipt の
  実証がない場合は recovery_required を維持する。PID 消失、時間経過、VM reboot だけで解除しない。
- terminal journal 後も bundle/receipt が一致しない場合は閉鎖を維持する。gate JSON の手編集や
  削除による「復旧」は行わない。helper を失った recover から GUI は自動復帰しない。
- gate 解放直後にも再び lock/logout/Quit/opt-out を発生させ、launch 直前の再確認で復帰を抑止する。

## 判定と後片付け

focused test、required e2e、gate の結果と native smoke の結果を別欄にする。
各項目を native 実行 / simulated / 未実施で分類し、最終の tested/reviewed commit を記録する。
spawn 成功は GUI/KLP 検証済みと同義ではない。未実施があれば end-to-end ready としない。
VM と一時鍵を破棄する。operator host への install、停止、restart、live queue の追加は行わない。
