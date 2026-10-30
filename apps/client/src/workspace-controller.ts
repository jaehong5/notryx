import type { WorkspaceSyncState } from "../../../packages/protocol-types";
import { WorkspaceListLoader } from "./workspace-list";

export class WorkspaceController {
  private readonly inventory = new WorkspaceListLoader();
  private inventoryRevision = 0;
  private syncRevision = 0;
  private sync: WorkspaceSyncState | null = null;

  invalidate(): void {
    this.inventory.invalidate();
    this.inventoryRevision++;
    this.syncRevision++;
    this.sync = null;
  }

  get connecting(): boolean { return this.sync?.status === "connecting"; }

  async readSync(read: () => Promise<WorkspaceSyncState>, isCurrent: () => boolean) {
    const revision = ++this.syncRevision;
    let current: WorkspaceSyncState;
    try {
      current = await read();
    } catch (cause) {
      if (revision !== this.syncRevision || !isCurrent()) return null;
      throw cause;
    }
    if (revision !== this.syncRevision || !isCurrent()) return null;
    const previous = this.sync;
    this.sync = current;
    return {
      state: current,
      refreshInventory: previous !== null && previous.status !== "ready" && current.status === "ready"
        && JSON.stringify(previous.binding) === JSON.stringify(current.binding),
    };
  }

  async loadInventory(read: Parameters<WorkspaceListLoader["load"]>[0], startAfter: string | null) {
    const revision = ++this.inventoryRevision;
    try {
      const result = await this.inventory.load(read, startAfter);
      return { ...result, revision };
    } catch (cause) {
      if (!this.isCurrentInventory(revision)) throw new Error("stale_workspace_list", { cause });
      throw cause;
    }
  }

  isCurrentInventory(revision: number): boolean { return revision === this.inventoryRevision; }
}
