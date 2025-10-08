# Makefile for time benchmark tests

CC = gcc
CFLAGS = -O2 -Wall -Wextra
LDFLAGS = -lrt

TARGET = time_benchmark
SOURCE = time_benchmark.c

all: $(TARGET)

$(TARGET): $(SOURCE)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

clean:
	rm -f $(TARGET)
	rm -rf shadow.data

test-native: $(TARGET)
	@echo "=== 原生Linux测试 ==="
	./$(TARGET) 100000

test-shadow: $(TARGET)
	@echo "=== Shadow模拟测试 ==="
	/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow_time_test.yaml

test-shadow-strace: $(TARGET)
	@echo "=== Shadow + Strace测试 ==="
	/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow --strace-logging-mode=standard shadow_time_test.yaml

analyze:
	@echo "=== 分析strace结果 ==="
	@for f in shadow.data/hosts/*/time_benchmark.*.strace; do \
		if [ -f "$$f" ]; then \
			echo "分析: $$f"; \
			python3 ../analyze_strace.py "$$f"; \
		fi; \
	done

.PHONY: all clean test-native test-shadow test-shadow-strace analyze

