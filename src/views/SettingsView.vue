<script setup lang="ts">
import { storeToRefs } from "pinia";
import { onMounted, ref, watch } from "vue";
import Button from "primevue/button";
import Dialog from "primevue/dialog";
import InputNumber from "primevue/inputnumber";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { open } from "@tauri-apps/plugin-dialog";
import { useAuthStore } from "../stores/auth";
import { DEFAULT_ARCHIVE_INTERVAL, MIN_ARCHIVE_INTERVAL, getArchiveInterval, resetAppSettings, setArchiveInterval } from "../utils/appSettings";
import { deleteAllAppData, getArchiveStorageInfo, setArchiveStorageDir, type ArchiveStorageInfo } from "../utils/qzone";
import { isWebDebugRuntime } from "../utils/runtime";

const authStore = useAuthStore();
const { loggedIn, user } = storeToRefs(authStore);
const intervalMs = ref(getArchiveInterval());
const privacyVisible = ref(false);
const deleteVisible = ref(false);
const deleting = ref(false);
const error = ref("");
const appVersion = ref("");
const storage = ref<ArchiveStorageInfo>();
const changingStorage = ref(false);
const storageNotice = ref("");

onMounted(async () => {
  try {
    appVersion.value = await getVersion();
  } catch (reason) {
    console.warn("读取应用版本失败", reason);
  }
  try { storage.value = await getArchiveStorageInfo(); }
  catch (reason) { error.value = `读取保存位置失败：${String(reason)}`; }
});

async function chooseStorageDirectory() {
  changingStorage.value = true; error.value = ""; storageNotice.value = "";
  try {
    const selected = isWebDebugRuntime
      ? "D:\\QQ空间归档（网页模拟）"
      : await open({ directory: true, multiple: false, title: "选择 QQ 空间归档保存目录" });
    if (!selected || Array.isArray(selected)) return;
    storage.value = await setArchiveStorageDir(selected);
    storageNotice.value = isWebDebugRuntime
      ? "网页模拟切换成功；真实目录只会在桌面版中创建。"
      : "保存位置已切换，旧数据库和媒体已复制到新目录；原目录仍保留，可确认无误后自行处理。";
  } catch (reason) { error.value = `切换保存位置失败：${String(reason)}`; }
  finally { changingStorage.value = false; }
}


watch(intervalMs, (value) => { intervalMs.value = setArchiveInterval(value); });

async function deleteEverything() {
  deleting.value = true; error.value = "";
  try {
    await deleteAllAppData();
    resetAppSettings(); intervalMs.value = DEFAULT_ARCHIVE_INTERVAL;
    await authStore.logout();
    deleteVisible.value = false;
  } catch (reason) { error.value = String(reason); }
  finally { deleting.value = false; }
}
</script>

<template>
  <section class="settings-stack">
    <article class="surface-card settings-card">
      <div class="settings-copy"><span class="settings-icon tone-blue"><i class="pi pi-user" /></span><div><h3>QQ 空间账号</h3><p>{{ loggedIn ? `${user?.nickname}（QQ ${user?.uin}）` : "尚未登录 QQ 空间" }}</p></div></div>
      <Button v-if="loggedIn" label="退出登录" icon="pi pi-sign-out" severity="danger" outlined @click="authStore.logout" />
      <Button v-else label="登录" icon="pi pi-link" @click="authStore.openLogin" />
    </article>

    <article class="surface-card settings-card interval-setting">
      <div class="settings-copy"><span class="settings-icon tone-green"><i class="pi pi-clock" /></span><div><h3>单页获取间隔</h3><p>每读取一页后等待一段时间再请求下一页，间隔越久越稳定。</p></div></div>
      <div class="interval-control"><InputNumber v-model="intervalMs" :min="MIN_ARCHIVE_INTERVAL" :max="30000" :step="500" suffix=" ms" show-buttons button-layout="horizontal" decrement-button-icon="pi pi-minus" increment-button-icon="pi pi-plus" /><small>最低 2000ms，建议 3000–5000ms</small></div>
    </article>

    <article class="surface-card settings-card storage-setting">
      <div class="settings-copy"><span class="settings-icon tone-blue"><i class="pi pi-folder" /></span><div><h3>归档保存位置</h3><p>{{ storage?.custom ? "当前使用你选择的自定义目录" : "当前使用系统应用数据目录" }}</p><small class="storage-current-path">{{ storage?.rootDir || "正在读取…" }}</small></div></div>
      <div class="storage-actions"><Button label="选择文件夹" icon="pi pi-folder-open" outlined :loading="changingStorage" @click="chooseStorageDirectory" /><small>切换时复制已有数据库、图片和视频，不删除旧目录。</small></div>
    </article>
    <p v-if="storageNotice" class="settings-success"><i class="pi pi-check-circle" />{{ storageNotice }}</p>

    <article class="surface-card settings-card">
      <div class="settings-copy"><span class="settings-icon tone-purple"><i class="pi pi-shield" /></span><div><h3>隐私协议</h3><p>了解登录凭证、归档内容和网络请求的处理方式。</p></div></div>
      <Button label="查看协议" icon="pi pi-angle-right" icon-pos="right" severity="secondary" text @click="privacyVisible = true" />
    </article>

    <article class="surface-card settings-card danger-settings-card">
      <div class="settings-copy"><span class="settings-icon tone-red"><i class="pi pi-trash" /></span><div><h3>删除所有数据</h3><p>删除全部账号的归档、续传记录、媒体缓存和本地登录状态。</p></div></div>
      <Button label="删除所有数据" icon="pi pi-trash" severity="danger" outlined @click="deleteVisible = true" />
    </article>

    <p v-if="error" class="archive-error"><i class="pi pi-exclamation-circle" />{{ error }}</p>
    <article class="surface-card settings-card about-card">
      <div class="about-main">
        <div class="settings-copy"><span class="settings-icon"><i class="pi pi-info-circle" /></span><div><h3>关于</h3><p>Qzone Archive · 跨平台空间归档工具</p><p class="author-line">作者：<button class="author-link" type="button" @click="openUrl('https://space.bilibili.com/1117414477')">LibraHp_0928 <i class="pi pi-external-link" /></button></p></div></div>
        <span class="version-badge">{{ appVersion ? `v${appVersion}` : "版本未知" }}</span>
      </div>
      <div class="sponsor-section">
        <div class="sponsor-heading"><div><h4>开源来源与致谢</h4><p>本版本基于 QzoneArchive 修改，保留 GPLv3。原作者与社区提供了核心归档实现。</p></div><i class="pi pi-heart-fill" /></div>
        <Button label="原项目与作者" text @click="openUrl('https://github.com/Gaoshu705/QzoneArchive')" />
        <Button label="Offline 衍生版本" text @click="openUrl('https://github.com/lix965996-art/QzoneArchive-Offline')" />
      </div>
    </article>
  </section>

  <Dialog v-model:visible="privacyVisible" modal :draggable="false" class="privacy-dialog" header="隐私协议">
    <div class="privacy-content">
      <p>空间归档是一款本地归档工具。我们重视你的账号与空间内容安全。</p>
      <h4>1. 数据存储</h4><p>QQ 空间动态、留言、点赞、评论、登录会话和媒体缓存保存在你的设备本地，不会上传至本项目的开发者服务器。</p>
      <h4>2. 网络请求</h4><p>应用仅在登录、读取空间资料、归档内容及下载相关媒体时直接请求腾讯 QQ、QQ 空间及其媒体域名。</p>
      <h4>3. 登录凭证</h4><p>扫码登录产生的 Cookie 仅用于访问当前账号有权查看的 QQ 空间内容。退出登录或删除所有数据后，本地会话会被清除。</p>
      <h4>4. 导出与分享</h4><p>导出的 HTML 和保存的图片由你自行保管。文件可能包含昵称、QQ 号、头像和空间内容，请谨慎分享。</p>
      <h4>5. 数据删除</h4><p>你可以随时使用“删除所有数据”清理全部本地归档、任务续传位置、视频缓存与登录状态，此操作无法撤销。</p>
    </div>
    <template #footer><Button label="我已了解" @click="privacyVisible = false" /></template>
  </Dialog>

  <Dialog v-model:visible="deleteVisible" modal :closable="!deleting" :draggable="false" class="delete-dialog" header="删除所有数据？">
    <div class="delete-dialog-content"><span class="delete-warning"><i class="pi pi-exclamation-triangle" /></span><div><p>所有账号的本地归档和媒体缓存都将被永久删除。</p><small>包括动态、留言、评论、点赞、续传记录、视频缓存及登录状态。此操作无法撤销。</small></div></div>
    <template #footer><Button label="取消" severity="secondary" text :disabled="deleting" @click="deleteVisible = false" /><Button label="确认全部删除" icon="pi pi-trash" severity="danger" :loading="deleting" @click="deleteEverything" /></template>
  </Dialog>
</template>
