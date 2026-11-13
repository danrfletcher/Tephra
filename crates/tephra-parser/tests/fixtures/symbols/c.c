#include <stdio.h>
#define LIMIT 10

/** A point. */
struct Point {
    int x;
    int y;
};

enum Color { RED, GREEN };

union Value { int i; float f; };

typedef struct {
    int w;
} Size;

int counter = 0;
static int hidden = 1;
const int ANSWER = 42;
extern int elsewhere;

int add(int a, int b);

/** Adds. */
int add(int a, int b) {
    int local = 1;
    return a + b;
}

static void helper(void) {}
