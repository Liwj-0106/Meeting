import { Download } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from './ui/dialog';
import { Button } from './ui/button';
import type { UpdateInfo } from '@/services/updateService';

interface UpdateDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  updateInfo: UpdateInfo | null;
}

export function UpdateDialog({ open, onOpenChange, updateInfo }: UpdateDialogProps) {
  if (!updateInfo?.available) return null;

  const guidance = updateInfo.portableMode
    ? '更新项目源码后运行 scripts/run-meetily-build-portable.cmd，再使用 run-meetily.cmd 启动。这样不会在系统目录中安装第二份 Meetily。'
    : '当前构建只允许检查版本，不具备下载或安装权限。请从可信发布渠道手动更新；更新前请确认所选方式不会改变现有数据目录。';

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[500px]">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Download className="h-5 w-5 text-blue-600" />
            有可用更新
          </DialogTitle>
          <DialogDescription>
            当前构建仅检查更新，不会在后台下载、安装或重启应用。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-2">
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">当前版本</span>
              <span className="font-medium">{updateInfo.currentVersion}</span>
            </div>
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">新版本</span>
              <span className="font-medium text-blue-600">{updateInfo.version}</span>
            </div>
            {updateInfo.date && (
              <div className="flex justify-between text-sm">
                <span className="text-muted-foreground">发布日期</span>
                <span className="font-medium">{formatDate(updateInfo.date)}</span>
              </div>
            )}
          </div>

          {updateInfo.body && (
            <div className="max-h-40 overflow-y-auto rounded-lg bg-gray-50 p-3">
              <p className="whitespace-pre-wrap text-sm text-gray-700">{updateInfo.body}</p>
            </div>
          )}

          <div className="rounded-lg border border-blue-200 bg-blue-50 p-3">
            <p className="text-sm text-blue-900">{guidance}</p>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function formatDate(dateString?: string): string {
  if (!dateString) return '';
  try {
    return new Date(dateString).toLocaleDateString();
  } catch {
    return dateString;
  }
}
