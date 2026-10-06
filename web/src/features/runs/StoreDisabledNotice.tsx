import { Empty } from '../../ui/Text';

export function StoreDisabledNotice() {
  return (
    <Empty>
      Run history is disabled on this server (persistence is off), so only live state is available.
    </Empty>
  );
}
