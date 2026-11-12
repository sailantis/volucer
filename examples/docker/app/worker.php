<?php

declare(strict_types=1);

require __DIR__ . '/vendor/autoload.php';

use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;

// `connect()`, not `create()`: when volucer starts this worker it sets `VOLUCER_WORKER_SOCKET`,
// and `connect()` reads it. `create()` reads `php://stdin`, which volucer has already closed.
Worker::connect()->run(static function (Request $request): Response {
    $path = (string) parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/health') {
        return Response::json([
            'status' => 'ok',
            'pid'    => getmypid(),
            'php'    => PHP_VERSION,
            'sapi'   => PHP_SAPI,
        ]);
    }

    return Response::text("hello from pid " . getmypid() . " on PHP " . PHP_VERSION . "\n");
});
