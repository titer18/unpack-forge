export interface ProgressPayload {
  percent: number;
  bytes: number | null;
  total_bytes: number | null;
  speed_bps: number | null;
  eta_secs: number | null;
  message: string | null;
  done: boolean;
  error: string | null;
}
