/*
 * Shadow时间调用性能测试
 * 对比不同频率的clock_gettime调用对性能的影响
 */

#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <string.h>

// 测试函数：频繁调用clock_gettime
void test_frequent_time_calls(int iterations) {
    struct timespec ts;
    for (int i = 0; i < iterations; i++) {
        clock_gettime(CLOCK_MONOTONIC, &ts);
    }
}

// 测试函数：稀疏调用clock_gettime
void test_sparse_time_calls(int iterations, int interval) {
    struct timespec ts;
    for (int i = 0; i < iterations; i++) {
        if (i % interval == 0) {
            clock_gettime(CLOCK_MONOTONIC, &ts);
        }
        // 模拟一些工作
        volatile int dummy = 0;
        for (int j = 0; j < 100; j++) {
            dummy += j;
        }
    }
}

// 测试函数：无时间调用（基线）
void test_no_time_calls(int iterations) {
    for (int i = 0; i < iterations; i++) {
        // 模拟一些工作
        volatile int dummy = 0;
        for (int j = 0; j < 100; j++) {
            dummy += j;
        }
    }
}

// 测试函数：使用缓存时间
static struct timespec cached_time;
void update_cached_time() {
    clock_gettime(CLOCK_MONOTONIC, &cached_time);
}

void test_cached_time_calls(int iterations, int update_interval) {
    update_cached_time();
    for (int i = 0; i < iterations; i++) {
        if (i % update_interval == 0) {
            update_cached_time();
        }
        // 直接读取缓存（模拟共享内存优化）
        struct timespec ts = cached_time;
        (void)ts; // 防止优化掉
        
        // 模拟一些工作
        volatile int dummy = 0;
        for (int j = 0; j < 100; j++) {
            dummy += j;
        }
    }
}

int main(int argc, char *argv[]) {
    int iterations = 100000;
    if (argc > 1) {
        iterations = atoi(argv[1]);
    }
    
    struct timespec start, end;
    double elapsed;
    
    printf("====================================\n");
    printf("Shadow时间调用性能测试\n");
    printf("迭代次数: %d\n", iterations);
    printf("====================================\n\n");
    
    // 测试1：频繁时间调用
    printf("[测试1] 频繁时间调用 (每次迭代都调用clock_gettime)\n");
    clock_gettime(CLOCK_MONOTONIC, &start);
    test_frequent_time_calls(iterations);
    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("  耗时: %.3f秒\n", elapsed);
    printf("  平均每次调用: %.0f纳秒\n\n", elapsed * 1e9 / iterations);
    
    // 测试2：稀疏时间调用 (每10次调用一次)
    printf("[测试2] 稀疏时间调用 (每10次迭代调用一次clock_gettime)\n");
    clock_gettime(CLOCK_MONOTONIC, &start);
    test_sparse_time_calls(iterations, 10);
    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("  耗时: %.3f秒\n", elapsed);
    printf("  clock_gettime调用次数: %d\n\n", iterations / 10);
    
    // 测试3：稀疏时间调用 (每100次调用一次)
    printf("[测试3] 稀疏时间调用 (每100次迭代调用一次clock_gettime)\n");
    clock_gettime(CLOCK_MONOTONIC, &start);
    test_sparse_time_calls(iterations, 100);
    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("  耗时: %.3f秒\n", elapsed);
    printf("  clock_gettime调用次数: %d\n\n", iterations / 100);
    
    // 测试4：无时间调用（基线）
    printf("[测试4] 无时间调用 (基线性能)\n");
    clock_gettime(CLOCK_MONOTONIC, &start);
    test_no_time_calls(iterations);
    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("  耗时: %.3f秒\n\n", elapsed);
    
    // 测试5：使用缓存时间（模拟共享内存优化）
    printf("[测试5] 缓存时间 (每100次迭代更新一次，模拟共享内存优化)\n");
    clock_gettime(CLOCK_MONOTONIC, &start);
    test_cached_time_calls(iterations, 100);
    clock_gettime(CLOCK_MONOTONIC, &end);
    elapsed = (end.tv_sec - start.tv_sec) + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("  耗时: %.3f秒\n", elapsed);
    printf("  clock_gettime调用次数: %d\n\n", iterations / 100);
    
    printf("====================================\n");
    printf("测试完成！\n");
    printf("====================================\n");
    
    // 保持运行一段时间以便观察
    sleep(1);
    
    return 0;
}

