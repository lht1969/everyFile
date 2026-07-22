import type { IndexStatus } from '../types';

interface StatusBarProps {
  message: string;
  indexStatus: IndexStatus;
  isAdmin: boolean;
  searchTime?: number | null;
}

function StatusBar({ message, indexStatus, isAdmin, searchTime }: StatusBarProps) {
  return (
    <div className="status-bar">
      <div className="status-left">
        <span className="status-message">{message}</span>
      </div>
      <div className="status-right">
        {searchTime !== null && searchTime !== undefined && (
          <span className="search-time">
            {searchTime < 1000 ? `${Math.round(searchTime)}ms` : `${(searchTime / 1000).toFixed(1)}s`}
          </span>
        )}
        {indexStatus.volumes.length > 0 && (
          <span className="volume-info">
            卷- {indexStatus.volumes.join(' ')}
          </span>
        )}
        <span className="index-status">
          {indexStatus.status === 'scanning' && indexStatus.progress < 1 ? (
            <>索引中...</>
          ) : (
            <>已索引 {indexStatus.file_count.toLocaleString()} 个文件</>
          )}
        </span>
        {indexStatus.last_update && (
          <span className="last-update">
            更新: {indexStatus.last_update}
          </span>
        )}
        <span className={`admin-badge ${isAdmin ? 'admin' : 'normal'}`}>
          {isAdmin ? '管理员' : '普通用户'}
        </span>
      </div>
    </div>
  );
}

export default StatusBar;
