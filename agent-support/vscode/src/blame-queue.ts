import * as vscode from "vscode";

export interface BlameTask<T> {
  uri: vscode.Uri;
  priority: 'high' | 'normal';
  abortController: AbortController;
  execute: (signal: AbortSignal) => Promise<T>;
  resolve: (result: T | null) => void;
  reject: (error: Error) => void;
}

/**
 * A priority queue that limits concurrent blame operations to MAX_CONCURRENT.
 * High priority tasks (current selection) jump to the front of the queue.
 * Supports cancellation for when tabs are closed.
 */
export class BlameQueue<T> {
  private static readonly MAX_CONCURRENT = 2;
  
  private queue: BlameTask<T>[] = [];
  private running: Map<string, BlameTask<T>> = new Map();
  
  /**
   * Enqueue a blame task. Returns a promise that resolves when the task completes.
   */
  public enqueue(
    uri: vscode.Uri,
    priority: 'high' | 'normal',
    execute: (signal: AbortSignal) => Promise<T>
  ): Promise<T | null> {
    // Cancel any existing task for this URI (we're replacing it)
    this.cancelForUri(uri);
    
    return new Promise((resolve, reject) => {
      const task: BlameTask<T> = {
        uri,
        priority,
        abortController: new AbortController(),
        execute,
        resolve,
        reject,
      };
      
      // High priority tasks go to the front of the queue
      if (priority === 'high') {
        this.queue.unshift(task);
      } else {
        this.queue.push(task);
      }
      
      this.processQueue();
    });
  }
  
  /**
   * Cancel any pending or running task for the given URI.
   * Called when a tab is closed.
   */
  public cancelForUri(uri: vscode.Uri): void {
    const uriString = uri.toString();
    
    // Cancel and remove from queue
    this.queue = this.queue.filter(task => {
      if (task.uri.toString() === uriString) {
        task.abortController.abort();
        task.resolve(null);
        return false;
      }
      return true;
    });
    
    // Cancel running task if exists
    const runningTask = this.running.get(uriString);
    if (runningTask) {
      runningTask.abortController.abort();
      // Don't remove from running map here - let the task complete and clean up
    }
  }
  
  /**
   * Cancel all pending and running tasks.
   */
  public cancelAll(): void {
    // Cancel all queued tasks
    for (const task of this.queue) {
      task.abortController.abort();
      task.resolve(null);
    }
    this.queue = [];
    
    // Cancel all running tasks
    for (const task of this.running.values()) {
      task.abortController.abort();
    }
  }
  
  /**
   * Prioritize a task for the given URI (move to front of queue).
   */
  public prioritize(uri: vscode.Uri): void {
    const uriString = uri.toString();
    const index = this.queue.findIndex(task => task.uri.toString() === uriString);
    
    if (index > 0) {
      const [task] = this.queue.splice(index, 1);
      task.priority = 'high';
      this.queue.unshift(task);
    }
  }
  
  private processQueue(): void {
    while (this.running.size < BlameQueue.MAX_CONCURRENT && this.queue.length > 0) {
      const task = this.queue.shift()!;
      const uriString = task.uri.toString();
      
      this.running.set(uriString, task);
      
      this.executeTask(task).finally(() => {
        this.running.delete(uriString);
        this.processQueue();
      });
    }
  }
  
  private async executeTask(task: BlameTask<T>): Promise<void> {
    try {
      if (task.abortController.signal.aborted) {
        task.resolve(null);
        return;
      }
      
      const result = await task.execute(task.abortController.signal);
      
      if (task.abortController.signal.aborted) {
        task.resolve(null);
      } else {
        task.resolve(result);
      }
    } catch (error) {
      if (task.abortController.signal.aborted) {
        task.resolve(null);
      } else {
        console.error('[autter] BlameQueue task error:', error);
        task.resolve(null);
      }
    }
  }
  
  /**
   * Get the number of pending tasks in the queue.
   */
  public get pendingCount(): number {
    return this.queue.length;
  }
  
  /**
   * Get the number of currently running tasks.
   */
  public get runningCount(): number {
    return this.running.size;
  }
}





