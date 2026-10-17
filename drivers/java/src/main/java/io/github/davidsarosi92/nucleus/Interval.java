package io.github.davidsarosi92.nucleus;

/** An interval: months, days and microseconds, kept apart (a month is not a fixed number of days). */
public record Interval(int months, int days, long micros) {}
