package main

import "fmt"

// Server serves.
type Server struct {
	addr string
}

type Handler interface {
	Serve()
}

type ID int

type (
	Left  struct{}
	Right int
)

const Version = "1"

const (
	A = iota
	B
)

var Debug = false

var (
	x, y = 1, 2
)

// Start starts.
func Start() {
	var local = 1
	fmt.Println(local)
}

func (s *Server) Run() {}

func (s Server) Name() string { return "" }
